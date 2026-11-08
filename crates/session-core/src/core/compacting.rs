//! A compaction in progress, off the actor's thread: the summary request runs on a worker thread while the actor keeps serving the operations that must not wait for them. `abort` cancels the
//! compaction at once (manual `/compact`, threshold and overflow compactions alike) and `state` reports
//! `isCompacting`, as AgentSession does (`abortCompaction`, `isCompacting`); every other input waits for the
//! compaction, in order, as it did while the actor was blocked. A prompt with no run in progress is refused, as
//! TypeScript refuses one during a manual compaction; a tree navigation is refused as `navigateTree` refuses it
//! while `isCompacting`; a `compact` aborts what is in progress first (`compact` awaits `abort()`), then runs. A
//! tree navigation's branch summary runs the same way (core::navigate). (Pi extensions' compaction hooks,
//! `session_before_compact` and `session_compact`, are not supported by ultron-rs.)

use super::*;
use crate::compaction_op;
use ai::retry::RetryPolicy;

/// The error a cancelled compaction ends with (AgentSession: `Compaction cancelled`).
pub(super) const CANCELLED: &str = "Compaction cancelled";
/// A prompt during a manual compaction (AgentSession `prompt`).
const PROMPT_WHILE_COMPACTING: &str =
    "Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.";
/// A prompt during a navigation (harness `LaneBusy`: the navigation is the lane's operation).
const LANE_BUSY: &str = "Lane \"main\" already has an active operation";
/// A tree navigation during a compaction or another navigation's summary (AgentSession `navigateTree`).
pub(super) const NAVIGATE_WHILE_COMPACTING: &str =
    "Wait for the current compaction or tree navigation to finish before navigating the session tree.";

/// Sends the actor its wake-up when the work ends, also when it panics.
struct Wake(Sender<Input>);

impl Drop for Wake {
    fn drop(&mut self) {
        let _ = self.0.send(Input::Woken);
    }
}

/// The session's model call for a summary, taken off the actor (`summarize` as the actor makes it).
pub(super) struct Summarizer {
    provider: Arc<Provider>,
    model: Model,
    api_key: Option<String>,
    thinking: String,
    session_id: String,
}

impl Summarizer {
    async fn stream(&self, request: compaction::SummaryRequest, signal: &AbortSignal) -> ai::AssistantMessage {
        let options = SimpleStreamOptions {
            base: StreamOptions {
                api_key: self.api_key.clone(),
                max_tokens: Some(request.max_tokens),
                cache_retention: Some(ai::CacheRetention::None),
                session_id: request.session_id.clone(),
                signal: Some(signal.clone()),
                ..Default::default()
            },
            reasoning: request.reasoning.clone(),
            ..Default::default()
        };
        let mut sink = |_: &ai::AssistantMessageEvent| {};
        self.provider
            .stream(&self.model, &request.context, &options, &mut sink)
            .await
    }

    /// One plain side request (crate::auto_refine): `prompt` under `system`, answered as text; no retry.
    pub(super) fn ask(&self, system: &str, prompt: &str) -> Result<String, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let context = ai::transcript::normalize_context(&ai::Context {
            system_prompt: Some(system.to_string()),
            messages: vec![ai::Message::User(ai::UserMessage {
                content: ai::UserMessageContent::Blocks(vec![ai::UserContent::Text(ai::TextContent {
                    text: prompt.to_string(),
                    text_signature: None,
                })]),
                timestamp: now_ms(),
            })],
            tools: None,
        });
        let request = compaction::SummaryRequest {
            context,
            max_tokens: match self.model.max_tokens {
                0 => 2048,
                max => max.min(2048),
            },
            reasoning: None,
            session_id: Some(self.session_id.clone()),
        };
        let response = runtime.block_on(self.stream(request, &AbortSignal::new()));
        if response.stop_reason != ai::StopReason::Stop {
            return Err(response.error_message.unwrap_or_else(|| "no answer".into()));
        }
        Ok(response
            .content
            .iter()
            .filter_map(|block| match block {
                ai::AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// `/bug`'s report summary of `messages` (`summarizeForBugReport`, `generateBugReportSummary`), cancelled by
    /// `signal`; a retryable provider error is retried under `retry` (`completeSummarization`'s retry policy).
    pub(super) fn bug_report(
        &self,
        messages: &[Value],
        hint: Option<&str>,
        retry: RetryPolicy,
        signal: &AbortSignal,
    ) -> Result<String, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let mut attempt = 1;
        loop {
            let request = compaction::bug_summary_request(
                messages,
                hint,
                &self.model,
                Some(&self.thinking),
                Some(&self.session_id),
                now_ms(),
            );
            let response = runtime.block_on(self.stream(request, signal));
            if !signal.aborted()
                && let Some(delay) = retry.retry_delay(attempt, &response)
            {
                wait_until(
                    now_ms().saturating_add(i64::try_from(delay).unwrap_or(i64::MAX)),
                    signal,
                );
                attempt += 1;
                continue;
            }
            if signal.aborted() {
                return Err("Bug report summary was cancelled".into());
            }
            return compaction::bug_summary_text(&response);
        }
    }

    /// The branch summary of `entries` (`generateBranchSummary`), cancelled by `signal`.
    pub(super) fn branch(
        &self,
        entries: &[Value],
        custom_instructions: Option<&str>,
        replace_instructions: bool,
        reserve_tokens: u64,
        signal: &AbortSignal,
    ) -> Attempt<compaction::BranchSummary> {
        if signal.aborted() {
            return (Err(CANCELLED.into()), None);
        }
        let options = compaction::BranchSummaryOptions {
            model: &self.model,
            custom_instructions,
            replace_instructions,
            reserve_tokens,
            now_ms: now_ms(),
        };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => return (Err(error.to_string()), None),
        };
        let mut last = None;
        let result = runtime.block_on(compaction::generate_branch_summary(
            entries,
            &options,
            async |request| {
                let response = self.stream(request, signal).await;
                last = Some(response.clone());
                response
            },
        ));
        let result = result.map_err(|error| match error {
            compaction::BranchSummaryError::Aborted => CANCELLED.to_string(),
            compaction::BranchSummaryError::Failed(message) => message,
        });
        (result, last)
    }

    /// Summarize `preparation`, cancelled by `signal`; the result and the last summary response.
    pub(super) fn run(
        &self,
        preparation: &compaction::CompactionPreparation,
        custom_instructions: Option<&str>,
        signal: &AbortSignal,
    ) -> Attempt<compaction::CompactionResult> {
        if signal.aborted() {
            return (Err(CANCELLED.into()), None);
        }
        let options = compaction::CompactOptions {
            model: &self.model,
            custom_instructions,
            thinking_level: Some(&self.thinking),
            session_id: Some(&self.session_id),
            now_ms: now_ms(),
        };
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => return (Err(error.to_string()), None),
        };
        let mut last = None;
        let result = runtime.block_on(compaction::compact(preparation, &options, async |request| {
            let response = self.stream(request, signal).await;
            last = Some(response.clone());
            response
        }));
        (result, last)
    }
}

/// One summary attempt's result and the last response it got (whether a failure is retryable).
pub(super) type Attempt<T> = (Result<T, String>, Option<ai::AssistantMessage>);

/// A summary attempt, run off the actor (again on a retry).
pub(super) type AttemptFn<T> = Arc<dyn Fn() -> Attempt<T> + Send + Sync>;

/// What the summary attempts journal between attempts (`summary.retry_wait`, then the next attempt).
pub(super) enum RetryStep<'a> {
    /// A retryable attempt failed: its response's usage, the error and when the next attempt may start.
    Wait {
        usage: Option<Value>,
        error: &'a str,
        not_before: i64,
    },
    /// The wait ended: the next attempt starts.
    Retry,
}

impl CompactionOp {
    /// Journals `step` of this operation's summary attempts.
    pub(super) fn journal_step(
        &mut self,
        session: &mut session_journal::Session,
        step: RetryStep<'_>,
    ) -> Result<(), String> {
        match step {
            RetryStep::Wait {
                usage,
                error,
                not_before,
            } => self.retry_wait(session, usage.as_ref(), error, not_before),
            RetryStep::Retry => self.retry(session),
        }
    }
}

/// Waits until `not_before` (milliseconds since the epoch) or until `signal` aborts.
fn wait_until(not_before: i64, signal: &AbortSignal) {
    let delay = u64::try_from(not_before.saturating_sub(now_ms())).unwrap_or(0);
    if delay == 0 || signal.aborted() {
        return;
    }
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
        // The timer is made inside the runtime (it needs its reactor).
        let _ = runtime.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(delay), signal.cancelled()).await
        });
    }
}

impl Core {
    /// The summary call as this session makes it now.
    pub(super) fn summarizer(&self) -> Summarizer {
        Summarizer {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api_key: self.keys.request_key(&self.model.provider),
            thinking: self.thinking.clone(),
            session_id: lock_session(&self.session).id().to_string(),
        }
    }

    /// Run `work` on its own thread while the actor serves what may not wait for it ([`Core::while_compacting`]);
    /// the other inputs are kept, in order, for after it. `Err` when the work panicked.
    pub(super) fn off_actor<T: Send + 'static>(
        &mut self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, String> {
        let wake = Wake(self.tx.clone());
        let handle = std::thread::spawn(move || {
            let _wake = wake;
            work()
        });
        // Without its input (a core driven directly, not by `run_loop`), the actor just waits.
        if let Some(inputs) = self.inputs.take() {
            while let Ok(input) = inputs.recv() {
                match input {
                    Input::Woken => break,
                    other => self.while_compacting(other),
                }
            }
            self.inputs = Some(inputs);
        }
        handle.join().map_err(|_| "The compaction worker failed".to_string())
    }

    /// An input that arrived while a compaction's work runs off the actor.
    fn while_compacting(&mut self, input: Input) {
        match input {
            Input::Op(Op::Abort, reply) => {
                self.abort();
                reply(Ok(None));
            }
            Input::Op(Op::State, reply) => reply(Ok(Some(self.state()))),
            Input::Op(Op::Prompt { .. } | Op::PromptAndWait { .. }, reply) if self.run.is_none() => {
                // AgentSession runs a prompt alongside a branch summary; the lane admits one operation at a time
                // (harness `acceptRun`: `LaneBusy`), and the navigation is the lane's operation.
                reply(Err(if self.navigating {
                    LANE_BUSY.into()
                } else {
                    PROMPT_WHILE_COMPACTING.into()
                }));
            }
            Input::Op(Op::Navigate { .. }, reply) => reply(Err(NAVIGATE_WHILE_COMPACTING.into())),
            // `compact` awaits `abort()` (the compaction, branch summary and run in progress) before it compacts.
            Input::Op(compact @ Op::Compact { .. }, reply) => {
                self.abort();
                self.deferred.push_back(Input::Op(compact, reply));
            }
            Input::ActivityTick => self.report_activity(),
            // Closing cancels the compaction; the close itself runs once it has ended.
            close @ (Input::Close(_) | Input::HandOver(_)) => {
                self.cancel_compaction();
                self.deferred.push_back(close);
            }
            other => self.deferred.push_back(other),
        }
    }

    /// `abort`: the compaction in progress is cancelled (a manual one's cancel request journaled first), then the
    /// run in progress is aborted.
    pub(super) fn abort(&mut self) {
        if let Some(signal) = self.compacting.clone()
            && !signal.aborted()
        {
            self.cancel_compaction();
            // A failed write leaves the operation running in the journal: its terminal transaction still ends it.
            if self.run.is_none() {
                let _ = compaction_op::request_cancel(&mut lock_session(&self.session), now_ms());
            }
        }
        if let Some(run) = self.run.as_mut()
            && !run.signal.aborted()
        {
            run.signal.abort();
            run.cancelled = true;
            run.journal.request_cancel(&mut lock_session(&self.session), now_ms());
            self.abort_turn();
        }
    }

    fn cancel_compaction(&self) {
        if let Some(signal) = &self.compacting {
            signal.abort();
        }
    }

    /// The retry policy of a summary attempt (`normalizedRetryPolicy`): settings.json's `retry`, off when this
    /// session turned automatic retries off.
    pub(super) fn summary_retry(&self) -> RetryPolicy {
        RetryPolicy::from_settings(&self.runtime.config.settings["retry"], self.auto_retry)
    }

    /// The summary attempts of operation `run_id`'s task `step` (`runStructuralGeneration` and its retries): each
    /// runs off the actor; a failure whose last response is a retryable provider error (`isRetryableAssistantError`)
    /// waits `retryDelayMs` under the policy (off the actor, ended by `abort`) and is tried again until the policy's
    /// attempts run out. `journal` records the wait and the next attempt. Emits the harness's `retry_scheduled`,
    /// `retry_start` and `retry_end`, and AgentSession's `summarization_retry_*` (`attempt_start` is the summary's
    /// `summarization_retry_attempt_start`). Answers the last attempt and its response.
    pub(super) fn summary_attempts<T: Send + 'static>(
        &mut self,
        signal: &AbortSignal,
        retry: RetryPolicy,
        ids: (&str, &str),
        attempt_start: WatchEvent,
        attempt: AttemptFn<T>,
        journal: &mut dyn FnMut(&mut Core, RetryStep<'_>) -> Result<(), String>,
    ) -> Attempt<T> {
        let mut scheduled = false;
        let outcome = self.attempt_loop(signal, retry, ids, &attempt_start, attempt, journal, &mut scheduled);
        // AgentSession's `onRetryFinished`: the retries ended, however they did.
        if scheduled {
            self.emit(WatchEvent::SummarizationRetryFinished { lane: MAIN_LANE.into() });
        }
        outcome
    }

    #[allow(clippy::too_many_arguments)]
    fn attempt_loop<T: Send + 'static>(
        &mut self,
        signal: &AbortSignal,
        retry: RetryPolicy,
        (run_id, step): (&str, &str),
        attempt_start: &WatchEvent,
        attempt: AttemptFn<T>,
        journal: &mut dyn FnMut(&mut Core, RetryStep<'_>) -> Result<(), String>,
        scheduled: &mut bool,
    ) -> Attempt<T> {
        let mut number = 1;
        loop {
            let run = attempt.clone();
            let (result, last) = self.off_actor(move || run()).unwrap_or_else(|error| (Err(error), None));
            if signal.aborted() {
                return (result, last);
            }
            let retrying = last.as_ref().and_then(|message| retry.retry_delay(number, message));
            let (error, delay) = match (&result, retrying) {
                (Err(error), Some(delay)) => (error.clone(), delay),
                _ => {
                    if number > 1 {
                        self.emit(WatchEvent::RetryEnd {
                            lane: MAIN_LANE.into(),
                            run_id: Some(run_id.into()),
                            step: step.into(),
                            attempt: number,
                            success: result.is_ok(),
                            final_error: result.as_ref().err().cloned(),
                        });
                    }
                    return (result, last);
                }
            };
            let not_before = now_ms().saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
            let usage = last.as_ref().and_then(|m| serde_json::to_value(&m.usage).ok());
            if let Err(write) = journal(
                self,
                RetryStep::Wait {
                    usage,
                    error: &error,
                    not_before,
                },
            ) {
                return (Err(write), last);
            }
            *scheduled = true;
            self.emit(WatchEvent::RetryScheduled {
                lane: MAIN_LANE.into(),
                run_id: Some(run_id.into()),
                step: step.into(),
                attempt: number + 1,
                max_attempts: retry.max_attempts(),
                delay_ms: delay,
                not_before,
                error_message: error.clone(),
            });
            // AgentSession's `onRetryScheduled` (`retryAssistantCall`): the retry's number, the retries allowed
            // (`maxRetries`), the delay and the provider's error.
            let provider_error = last
                .as_ref()
                .and_then(|m| m.error_message.clone())
                .unwrap_or_else(|| error.clone());
            self.emit(WatchEvent::SummarizationRetryScheduled {
                lane: MAIN_LANE.into(),
                attempt: number,
                max_attempts: retry.max_attempts() - 1,
                delay_ms: delay,
                error_message: provider_error,
            });
            let cancel = signal.clone();
            let _ = self.off_actor(move || wait_until(not_before, &cancel));
            if signal.aborted() {
                return (Err(CANCELLED.into()), None);
            }
            number += 1;
            self.emit(WatchEvent::RetryStart {
                lane: MAIN_LANE.into(),
                run_id: Some(run_id.into()),
                step: step.into(),
                attempt: number,
            });
            self.emit(attempt_start.clone());
            if let Err(write) = journal(self, RetryStep::Retry) {
                return (Err(write), None);
            }
        }
    }

    /// A manual compaction: `compaction_start`, the journaled operation (the summary, cancellable), the entry, `compaction_end`; answers Pi's `CompactionResult` fields of the compaction entry.
    pub(super) fn compact(&mut self, instructions: Option<&str>) -> Result<Value, String> {
        self.idle()?;
        self.emit(WatchEvent::CompactionStart {
            lane: MAIN_LANE.into(),
            run_id: None,
            reason: "manual".into(),
        });
        let signal = AbortSignal::new();
        self.compacting = Some(signal.clone());
        let outcome = self.compact_now(instructions, &signal);
        let id = match outcome {
            Ok(id) => id,
            Err(error) => {
                // AgentSession clears its compaction state before `compaction_end`.
                self.compacting = None;
                let cancelled = error == CANCELLED;
                self.emit(WatchEvent::CompactionEnd {
                    lane: MAIN_LANE.into(),
                    run_id: None,
                    reason: "manual".into(),
                    status: if cancelled { Ended::Aborted } else { Ended::Failed },
                    error: (!cancelled).then(|| Failure {
                        code: None,
                        message: error.clone(),
                    }),
                });
                return Err(error);
            }
        };
        self.announce();
        self.compacting = None;
        self.emit(WatchEvent::CompactionEnd {
            lane: MAIN_LANE.into(),
            run_id: None,
            reason: "manual".into(),
            status: Ended::Completed,
            error: None,
        });
        self.retain_compaction();
        let pi = self.pi();
        let entry = pi
            .entry(&id)
            .filter(|e| e["type"] == "compaction")
            .ok_or("Compaction produced no compaction entry")?;
        let mut data = Map::new();
        for key in ["summary", "firstKeptEntryId", "tokensBefore", "usage", "details"] {
            if let Some(value) = entry.get(key).filter(|v| !v.is_null()) {
                data.insert(key.into(), value.clone());
            }
        }
        Ok(Value::Object(data))
    }

    /// Summarize the current path and commit the native compaction entry (its retained tail is the kept messages)
    /// through a journaled compaction operation ([`CompactionOp`]); returns its id. Nothing to cut keeps the whole
    /// path after an (empty-conversation) summary, as the native harness does. A cancelled compaction ends its
    /// operation `aborted` with [`CANCELLED`].
    fn compact_now(&mut self, custom_instructions: Option<&str>, signal: &AbortSignal) -> Result<String, String> {
        let prepared = self.prepare_compaction(compaction::CompactionSettings::default())?;
        let mut op = CompactionOp::begin(
            &mut lock_session(&self.session),
            &prepared.preparation,
            &prepared.retained_tail,
            custom_instructions,
        )?;
        if signal.aborted() {
            op.abort(&mut lock_session(&self.session))?;
            return Err(CANCELLED.into());
        }
        let retry = self.summary_retry();
        op.request(&mut lock_session(&self.session), retry)?;
        let summarizer = self.summarizer();
        let (preparation, instructions, cancel) = (
            prepared.preparation.clone(),
            custom_instructions.map(str::to_string),
            signal.clone(),
        );
        let attempt: AttemptFn<compaction::CompactionResult> =
            Arc::new(move || summarizer.run(&preparation, instructions.as_deref(), &cancel));
        let ids = (op.operation_id().to_string(), op.task_id().to_string());
        let mut journal =
            |core: &mut Core, step: RetryStep<'_>| op.journal_step(&mut lock_session(&core.session), step);
        let (result, _) = self.summary_attempts(
            signal,
            retry,
            (&ids.0, &ids.1),
            WatchEvent::SummarizationRetryAttemptStart {
                lane: MAIN_LANE.into(),
                source: "compaction".into(),
                reason: Some("manual".into()),
            },
            attempt,
            &mut journal,
        );
        if signal.aborted() {
            op.abort(&mut lock_session(&self.session))?;
            return Err(CANCELLED.into());
        }
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                op.fail(&mut lock_session(&self.session), &error)?;
                return Err(error);
            }
        };
        let usage = serde_json::to_value(&result.usage).map_err(|e| e.to_string())?;
        op.record_usage(&mut lock_session(&self.session), &usage)?;
        op.complete(
            &mut lock_session(&self.session),
            result.entry(prepared.retained_tail, usage),
        )
    }
}
