//! Run admission, agent-event projection, retries, queues, recovery and user bash for [`super::Core`].

use super::*;

impl Core {
    fn journal(&mut self, event: &AgentEvent) {
        if let Some(run) = self.run.as_mut() {
            run.journal.event(&mut lock_session(&self.session), event);
        }
    }

    pub(super) fn on_agent_event(&mut self, agent_event: AgentEvent) {
        let run = self.run_id();
        match &agent_event {
            AgentEvent::MessageStart { message } => {
                let value = stored(message);
                let aborted = self.run.as_ref().is_some_and(|r| r.signal.aborted());
                if aborted
                    && value["role"] == "assistant"
                    && value["stopReason"] == "aborted"
                    && value["content"].as_array().is_none_or(Vec::is_empty)
                {
                    if let Some(run) = self.run.as_mut() {
                        run.hiding = true;
                    }
                    return;
                }
                self.journal(&agent_event);
                let shown = self.persona_shown(value);
                self.emit(WatchEvent::MessageStart {
                    run_id: run,
                    lane: MAIN_LANE.into(),
                    message: shown,
                });
            }
            AgentEvent::MessageUpdate {
                message,
                assistant_message_event,
            } => {
                if self.run.as_ref().is_some_and(|run| run.hiding) {
                    return;
                }
                self.journal(&agent_event);
                let (frame, usage) = crate::journal::update_fields(message, assistant_message_event);
                self.emit(WatchEvent::MessageUpdate {
                    run_id: run,
                    lane: MAIN_LANE.into(),
                    frame,
                    usage,
                    message: None,
                });
            }
            AgentEvent::MessageEnd { message } => {
                if self.run.as_mut().is_some_and(|run| std::mem::take(&mut run.hiding)) {
                    return;
                }
                let value = stored(message);
                if value["role"] == "user" {
                    return self.user_message_end(value, None);
                }
                if value["role"] == "assistant"
                    && let Some(running) = self.run.as_mut()
                {
                    running.error = value["errorMessage"].as_str().map(str::to_string);
                }
                let shown = self.persona_shown(value.clone());
                self.emit(WatchEvent::MessageEnd {
                    run_id: run,
                    lane: MAIN_LANE.into(),
                    message: shown,
                });
                let answered = value["role"] == "assistant";
                let round_message = Some(value.clone());
                let mut retry = None;
                if let Message::Assistant(answer) = message {
                    retry = self.retry_decision(answer);
                }
                if value["role"] == "assistant" {
                    self.persona_assistant(&value);
                    self.turn_guard_assistant(&value);
                }
                self.journal(&agent_event);
                self.announce();
                let retried = matches!(retry, Some(WatchEvent::RetryScheduled { .. }));
                if let Some(note) = retry {
                    self.emit(note);
                }
                if let Some(message) = round_message.filter(|_| !retried) {
                    self.count_round(&message);
                }
                if answered {
                    let _ = self.context.message_end(MAIN_LANE, "assistant");
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.journal(&agent_event);
                self.emit(WatchEvent::ToolStart {
                    run_id: run,
                    lane: MAIN_LANE.into(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                });
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => self.emit(WatchEvent::ToolUpdate {
                run_id: run,
                lane: MAIN_LANE.into(),
                tool_call_id: tool_call_id.clone(),
                tool_name: tool_name.clone(),
                args: args.clone(),
                partial_result: serde_json::to_value(partial_result).unwrap_or(Value::Null),
            }),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
            } => {
                let mut result = serde_json::to_value(result).unwrap_or(Value::Null);
                if *is_error && result["details"] == json!({}) {
                    result.as_object_mut().map(|m| m.remove("details"));
                }
                self.emit(WatchEvent::ToolEnd {
                    run_id: run,
                    lane: MAIN_LANE.into(),
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    result,
                    is_error: *is_error,
                });
            }
            AgentEvent::TurnStart => self.begin_generation(),
            AgentEvent::AgentStart | AgentEvent::AgentEnd { .. } | AgentEvent::TurnEnd { .. } => {}
        }
    }

    /// A generation begins. The queued messages the loop took for it (drained before its `turn_start`) join the
    /// branch in the same transaction as `assistant.ready`, and the lane's inbox keeps the rest. A cancelled run
    /// starts no generation.
    ///
    /// Only messages taken for this turn or an earlier one: the actor processes `turn_start` after the loop emitted
    /// it, and by then a fast loop may already have drained the next turn's follow-up. Taking every delivered message
    /// committed that follow-up before this turn's assistant message (seen under load as `steer`, `follow-up`,
    /// `assistant`, `assistant` instead of `steer`, `assistant`, `follow-up`, `assistant`).
    fn begin_generation(&mut self) {
        let Some(run) = self.run.as_mut() else { return };
        run.turns += 1;
        let turn = run.turns;
        if run.signal.aborted() {
            return;
        }
        let taken: Vec<Queued> = sync::lock(&self.queues)
            .delivered
            .iter()
            .filter(|q| q.turn <= turn && !self.taken.contains(&q.entry_id))
            .cloned()
            .collect();
        let tip = self.tip();
        let (mut writes, mut parent) = context_lane::placement(&lock_session(&self.session), tip.clone());
        for queued in &taken {
            writes.push(Write::message_entry(
                &queued.entry_id,
                parent.as_deref(),
                queued.message.clone(),
            ));
            writes.push(Write::delete(PENDING_ENTRY, &queued.entry_id));
            parent = Some(queued.entry_id.clone());
        }
        let trigger = taken.last().map(|q| q.entry_id.clone());
        let retry_start = self
            .run
            .as_ref()
            .and_then(|run| run.journal.run())
            .and_then(|journal| Some((journal.retry_waiting()?, journal.step_id().to_string())));
        if let Some((attempt, step)) = retry_start {
            let run_id = self.run_id();
            self.emit(WatchEvent::RetryStart {
                lane: MAIN_LANE.into(),
                run_id,
                step,
                attempt,
            });
        }
        if let Some(placed) = parent.filter(|placed| Some(placed) != tip.as_ref()) {
            writes.push(Write::set(BRANCH_TIP, MAIN_LANE, placed.into()));
        }
        let mut inbox = self.lane_state()["inbox"].clone();
        if let Some(items) = inbox.as_array_mut() {
            items.retain(|item| !taken.iter().any(|q| item["entryId"] == q.entry_id.as_str()));
        }
        self.taken.extend(taken.iter().map(|q| q.entry_id.clone()));
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(inbox);
            run.journal
                .begin_with(&mut lock_session(&self.session), writes, trigger);
        }
    }

    /// A journal write failed: the run stops (its error is reported when it ends).
    pub(super) fn fail_run(&mut self, error: String) {
        if let Some(run) = self.run.as_mut() {
            run.signal.abort();
            run.error.get_or_insert(error);
        }
    }

    /// The run ends: its last boundary, then its terminal transaction, then its end is reported. The run is taken
    /// only once the terminal transaction committed: when it fails, the run stays (`ending`) and the transaction is
    /// tried again on the next activity tick, so a run is never reported ended while its operation stays open (a
    /// successor would end it a second time).
    pub(super) fn run_end(&mut self) {
        self.observer.cancel();
        let operation = self.operation_id();
        let Some(ending) = self.run.as_ref().map(|run| run.ending) else {
            return;
        };
        if !ending {
            if let Some(operation) = &operation {
                self.lane.stack.agents.end_root_turn(operation);
            }
            let unanswered = self
                .run
                .as_ref()
                .filter(|run| !run.cancelled && run.error.is_none())
                .and_then(|run| self.unanswered(run.submission));
            let _ = self.context.run_end(MAIN_LANE);
            let waiting = context_lane::pending_count(&lock_session(&self.session));
            let placed = self.branches.place(true);
            if let Some(run) = self.run.as_mut() {
                run.ending = true;
                if let Some(unanswered) = unanswered {
                    run.error = Some(unanswered);
                }
                if let Err(error) = placed {
                    run.error.get_or_insert(error);
                }
                if let Some(write) = run.journal.error() {
                    run.error.get_or_insert(write.to_string());
                }
            }
            self.announce();
            if waiting > 0 {
                self.queue_update();
            }
            self.durable.bridge.set_user_abort(false);
        }
        let Some(run) = self.run.as_ref() else { return };
        let aborted = run.cancelled;
        let finished = if aborted {
            run.journal.finish_aborted(&mut lock_session(&self.session))
        } else {
            let code = if run.compaction_failed {
                "summarization_failed"
            } else {
                "assistant_error"
            };
            let failure = run.error.as_deref().map(|m| (code, m));
            run.journal.finish(Some(&mut lock_session(&self.session)), failure)
        };
        if finished.is_err() {
            return;
        }
        let Some(run) = self.run.take() else { return };
        let error = if aborted { None } else { run.error.clone() };
        self.announce();
        self.taken.clear();
        sync::lock(&self.queues).delivered.clear();
        let status = if aborted {
            Ended::Aborted
        } else if error.is_some() {
            Ended::Failed
        } else {
            Ended::Completed
        };
        self.report_activity();
        self.emit(WatchEvent::RunEnd {
            lane: MAIN_LANE.into(),
            run_id: run.id.clone(),
            status,
            error: error.as_ref().map(|message| Failure {
                code: Some("run_failed".into()),
                message: message.clone(),
            }),
            from_tip_id: run.started_tip.as_str().map(str::to_string),
            tip_id: self.tip(),
            ended_at: now_ms(),
        });
        let failed = error.is_some();
        if let Some(operation) = &operation {
            self.events.run_ended(MAIN_LANE, operation, aborted);
        }
        self.nudger.run_ended(&run.id);
        self.skill_nudger.run_ended(&run.id);
        self.lane.stack.stats.flush();
        self.lane.stack.cell_hints().run_ended(MAIN_LANE);
        if let Some(waiter) = run.waiter {
            let error = error.map_or(Value::Null, |m| json!({"code": "run_failed", "message": m}));
            waiter(Ok(Some(
                json!({"accepted": true, "operationId": run.id, "error": error}),
            )));
        }
        for input in std::mem::take(&mut self.after_run).into_iter().rev() {
            self.deferred.push_front(input);
        }
        self.persona_run_ended(aborted, failed);
        self.turn_guard_run_ended(aborted, failed);
        self.observe_mined_procedures(aborted, failed);
    }

    /// What a response means for the generation's retries (the harness's `publishResponse`): a retryable error
    /// within the policy's attempts is retried after `retryDelayMs` (journaled as `assistant.retry_wait`); a later
    /// attempt's response ends the retries: `retry_scheduled` / `retry_end` of the run's generation.
    fn retry_decision(&mut self, answer: &ai::AssistantMessage) -> Option<WatchEvent> {
        let policy = self.summary_retry();
        let run = self.run.as_mut()?;
        if run.signal.aborted() {
            return None;
        }
        let journal = run.journal.run()?;
        let (attempt, step) = (journal.attempt(), journal.step_id().to_string());
        if !harness::chat::models::is_context_overflow(answer)
            && let Some(delay) = policy.retry_delay(attempt, answer)
        {
            let not_before = now_ms().saturating_add(i64::try_from(delay).unwrap_or(i64::MAX));
            let message = answer
                .error_message
                .clone()
                .unwrap_or_else(|| "Assistant request failed".into());
            run.journal.schedule_retry(not_before, message.clone());
            return Some(WatchEvent::RetryScheduled {
                lane: MAIN_LANE.into(),
                run_id: Some(run.id.clone()),
                step,
                attempt: attempt + 1,
                max_attempts: policy.max_attempts(),
                delay_ms: delay,
                not_before,
                error_message: message,
            });
        }
        if attempt <= 1 {
            return None;
        }
        let success = !matches!(answer.stop_reason, ai::StopReason::Error | ai::StopReason::Aborted);
        let final_error = (!success).then(|| {
            answer.error_message.clone().unwrap_or_else(|| {
                let reason = serde_json::to_value(answer.stop_reason).unwrap_or(Value::Null);
                format!("Assistant request ended with {}", reason.as_str().unwrap_or_default())
            })
        });
        Some(WatchEvent::RetryEnd {
            lane: MAIN_LANE.into(),
            run_id: Some(run.id.clone()),
            step,
            attempt,
            success,
            final_error,
        })
    }

    /// A model turn of the run that answers without tools ends with its answer (`turn_end`); a tool round's turn
    /// ends with its last cell ([`crate::turns::Request::RoundEnded`]).
    fn count_round(&mut self, message: &Value) {
        if self.run.is_none() || message["role"] != "assistant" {
            return;
        }
        let calls = message["content"]
            .as_array()
            .map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count());
        if calls == 0 {
            self.turn_ended(0, 0);
        }
    }

    /// `turn_end` on `main`: the brake and the skill hint steer the run when due.
    pub(super) fn turn_ended(&mut self, calls: usize, failed: usize) {
        let Some(run) = self.run.as_ref().map(|r| r.id.clone()) else {
            return;
        };
        let running = self.root_started_running();
        let steers: Vec<String> = [
            self.nudger.turn_ended(&run, calls, running),
            self.skill_nudger.turn_ended(&run, calls, failed, running),
        ]
        .into_iter()
        .flatten()
        .collect();
        for steer in steers {
            self.lane.stack.stats.nudge(&steer);
            let _ = self.enqueue(QueueKind::Steer, steer, Vec::new(), None);
        }
    }

    /// `pendingRootNotifications`: subagents and root-level tasks the root started that have not ended.
    fn root_started_running(&self) -> u64 {
        use kernel_apis::TaskHost;
        let subagents = self.lane.stack.delegation.running(MAIN_LANE).len();
        let tasks = self
            .lane
            .stack
            .agents
            .tasks()
            .unwrap_or_default()
            .iter()
            .filter(|task| task["parentId"].is_null() && task["result"].is_null())
            .count();
        (subagents + tasks) as u64
    }

    /// Start a run of `message`; `waiter` is answered when it ends. On failure the waiter comes back with the error.
    pub(super) fn prompt(
        &mut self,
        message: String,
        images: Vec<Value>,
        waiter: Option<Reply>,
    ) -> Result<(), (String, Reply)> {
        self.prompt_with(user_message(message, images), waiter, true)
    }

    /// Start a run of the user message `prompt` (see [`Core::prompt`]).
    ///
    /// `take_queued`: the admission also takes the steering and follow-up messages queued while the lane was idle
    /// (by the queue modes); messages queued for the next run are always taken.
    pub(super) fn prompt_with(
        &mut self,
        prompt: Value,
        waiter: Option<Reply>,
        take_queued: bool,
    ) -> Result<(), (String, Reply)> {
        let fail = |error: String, waiter: Option<Reply>| -> Result<(), (String, Reply)> {
            match waiter {
                Some(waiter) => Err((error, waiter)),
                None => Err((error, Box::new(|_| {}))),
            }
        };
        let api_key = self.api_key();
        if api_key.is_none() && !lane::header_auth(&self.env, &self.model.provider) {
            return fail(format!("No API key found for {}", self.model.provider), waiter);
        }
        let text = message_text(&prompt);
        let continuation = prompt.get(crate::recovery::RECOVERY_DEPTH).is_some();
        let mut naming_request = None;
        let auto_name = self.auto_name
            && !continuation
            && prompt["role"] != "custom"
            && self.runtime.mode != "rpc"
            && !self.env.flag_off(AUTO_NAME_ENV);
        let named = if auto_name {
            names::apply_auto_name(&mut lock_session(&self.session), &text)
        } else {
            Ok(None)
        };
        match named {
            Ok(Some(_)) => {
                self.name_changed();
                let naming = naming::naming_settings(&self.runtime.config.settings);
                if naming.enabled {
                    let named = naming
                        .model
                        .as_deref()
                        .and_then(|spec| spec.split_once('/'))
                        .and_then(|(provider, id)| lane::catalog_model(provider, id, &self.env).ok());
                    let naming_model = named.unwrap_or_else(|| {
                        let chosen = naming::naming_model(naming.model.as_deref(), &self.model, &api_supported);
                        if chosen.provider == self.model.provider && chosen.id == self.model.id {
                            return self.model.clone();
                        }
                        lane::catalog_model(&chosen.provider, &chosen.id, &self.env).unwrap_or(chosen)
                    });
                    let key = self.keys.request_key(&naming_model.provider);
                    naming_request = Some((naming_model, key, text.clone()));
                }
            }
            Ok(None) => {}
            Err(error) => return fail(error, waiter),
        }
        let mut opening: Vec<Value> = self
            .loki_notice
            .take(crate::loki_notice::NOTICE_WAIT)
            .map(|text| crate::loki_notice::notice_message(&text, now_ms() as u64))
            .into_iter()
            .chain(
                self.memory_notice
                    .take()
                    .map(|text| crate::auto_memory::paused_message(&text, now_ms() as u64)),
            )
            .collect();
        if prompt["role"] == "user" && !continuation {
            opening.extend(self.recalled_memory(&text));
            opening.extend(self.routed_playbook(&text));
            opening.extend(self.refinements_message(&text));
            opening.extend(self.mined_procedure_message(&text));
        }
        for message in opening {
            self.announce_message(&message);
            if let Err(error) = self.append(message) {
                return fail(error, waiter);
            }
        }
        if let Err(error) = self.store_lane_config() {
            return fail(error, waiter);
        }
        let lane_config = lock_session(&self.session)
            .value(LANE_CONFIG, MAIN_LANE)
            .cloned()
            .unwrap_or(Value::Null);
        let started_tip = self.tip().map_or(Value::Null, Value::from);
        let settings = self.run_settings();
        let captured: Vec<(String, Value)> = select_accepted(&sync::lock(&self.queues), take_queued);
        let mut inbox = self.lane_state()["inbox"].clone();
        if let Some(items) = inbox.as_array_mut() {
            items.retain(|item| !captured.iter().any(|(id, _)| item["entryId"] == id.as_str()));
        }
        let mut journal = match RunJournal::admit(
            Some(&mut lock_session(&self.session)),
            lane_config,
            prompt.clone(),
            (self.model.max_tokens, self.model.context_window),
            settings,
            &captured,
            inbox,
        ) {
            Ok(journal) => journal,
            Err(error) => return fail(error, waiter),
        };
        if !captured.is_empty() {
            sync::lock(&self.queues)
                .items
                .retain(|q| !captured.iter().any(|(id, _)| *id == q.entry_id));
            self.queue_update();
        }
        let captured: Vec<Value> = captured.into_iter().map(|(_, message)| message).collect();
        if let Some((naming_model, key, text)) = naming_request {
            self.naming = Some(naming::Pending::spawn(
                self.provider.clone(),
                naming_model,
                key,
                text,
                self.tx.clone(),
            ));
        }
        journal.ledger_counts_turns();
        journal.set_retry_policy(self.summary_retry().journal_value());
        if let Some(options) = self.provider.stream_options() {
            journal.set_stream_options(options);
        }
        if let Some(run) = journal.run() {
            self.lane.stack.agents.begin_root_turn(run.operation_id());
            self.durable.children.set_root(
                self.lane
                    .stack
                    .agents
                    .usage_root_for_lane(MAIN_LANE, run.operation_id()),
            );
        }
        if self.model_unavailable {
            let identity = json!({"provider": self.model.provider, "modelId": self.model.id});
            let result = journal.fail_configuration(
                &mut lock_session(&self.session),
                "model_unavailable",
                MODEL_UNAVAILABLE,
                identity,
            );
            self.announce();
            return fail(result.err().unwrap_or_else(|| MODEL_UNAVAILABLE.to_string()), waiter);
        }
        let prompt_entry = self.tip().unwrap_or_default();
        if !continuation && prompt["role"] == "user" {
            self.checkpoint_turn(&prompt_entry);
        }
        let submission = match self.start_turn(&prompt, &prompt_entry) {
            Ok(submission) => submission,
            Err(error) => {
                let _ = journal.finish(Some(&mut lock_session(&self.session)), Some(("run_failed", &error)));
                return fail(error, waiter);
            }
        };
        let signal = AbortSignal::new();
        let id = jsrt::random::uuid_v7(now_ms());
        self.branches.run_started();
        self.lane.stack.begin_turn();
        self.lane.stack.stats.touch();
        self.run = Some(Running {
            id: id.clone(),
            signal,
            journal,
            prompt,
            captured,
            submission: Some(submission),
            prompt_pending: true,
            turns: 0,
            started_tip,
            error: None,
            waiter,
            cancelled: false,
            compaction_failed: false,
            resume_overflow: None,
            hiding: false,
            ending: false,
        });
        self.persona_run_started();
        self.turn_guard_cancel();
        self.emit(WatchEvent::RunStart {
            run_id: id,
            started_at: now_ms(),
            lane: MAIN_LANE.into(),
        });
        self.spawn_waiter();
        Ok(())
    }

    /// [`Op::Recover`]: start what the dead worker left: the first queued message, else the interrupted run's
    /// continuation (unless it was interrupted [`crate::recovery::MAX_RECOVERY_DEPTH`] times in a row, or
    /// `ULTRON_RECOVERY_RESUME` is `off`).
    pub(super) fn recover(&mut self) -> OpResult {
        let Some(recovered) = self.recovered.take() else {
            return Ok(Some(json!({"resumed": false, "reason": "nothing to recover"})));
        };
        let summary = json!({
            "operationId": recovered.operation_id, "status": recovered.status, "depth": recovered.depth,
            "interruptedCalls": recovered.interrupted_calls, "queued": recovered.queued.len(),
        });
        let answer = |resumed: bool, run: Value, reason: &str| {
            Ok(Some(
                json!({"resumed": resumed, "runId": run, "reason": reason, "recovered": summary}),
            ))
        };
        if let Some(run) = self.resumed.take() {
            return answer(true, json!(run), "durable run resumed");
        }
        if self.run.is_some() {
            return answer(false, Value::Null, "a run is in progress");
        }
        let resume_off = self.env.flag_off(RECOVERY_RESUME_ENV);
        let first = sync::lock(&self.queues)
            .items
            .iter()
            .find(|q| q.kind != QueueKind::NextRun)
            .cloned();
        let prompt = match first {
            _ if recovered.resume && !resume_off => {
                let mut prompt = user_message(crate::recovery::CONTINUE_PROMPT.to_string(), Vec::new());
                prompt[crate::recovery::RECOVERY_DEPTH] = json!(recovered.depth + 1);
                Some((prompt, "interrupted run"))
            }
            Some(queued) if recovered.status.as_deref() != Some("aborted") => {
                self.remove_queued(std::slice::from_ref(&queued.entry_id))?;
                let mut prompt = queued.message;
                prompt["timestamp"] = json!(now_ms());
                Some((prompt, "queued message"))
            }
            _ => None,
        };
        let Some((prompt, reason)) = prompt else {
            let reason = if recovered.operation_id.is_none() {
                "nothing to recover"
            } else if recovered.resume {
                "resuming is off"
            } else {
                "the interrupted run is not resumed"
            };
            return answer(false, Value::Null, reason);
        };
        match self.prompt_with(prompt, None, false) {
            Ok(()) => answer(true, json!(self.run_id()), reason),
            Err((error, _)) => Err(error),
        }
    }

    pub(super) fn enqueue(
        &mut self,
        kind: QueueKind,
        message: String,
        images: Vec<Value>,
        entry_id: Option<String>,
    ) -> Result<String, String> {
        if message.is_empty() && images.is_empty() {
            return Err(EMPTY_QUEUED.into());
        }
        let message = user_message(message, images);
        let entry_id =
            entry_id.unwrap_or_else(|| session_journal::lock_clock(&lock_session(&self.session).clock()).next_id());
        let mut lane_state = self.lane_state();
        if let Some(inbox) = lane_state["inbox"].as_array_mut() {
            inbox.push(json!({"entryId": entry_id, "kind": kind.name()}));
        }
        if let Some(run) = self.run.as_mut() {
            run.journal.set_inbox(lane_state["inbox"].clone());
        }
        lock_session(&self.session).commit(&[
            Write::set(PENDING_ENTRY, &entry_id, json!({"type": "message", "payload": message})),
            Write::set(LANE_STATE, MAIN_LANE, lane_state),
        ])?;
        sync::lock(&self.queues).items.push(Queued {
            kind,
            message,
            entry_id: entry_id.clone(),
            turn: 0,
            submission: None,
        });
        if self.run.is_some() {
            self.submit_queued()?;
        }
        self.queue_update();
        Ok(entry_id)
    }

    /// Remove queued messages (their pending entries and inbox items) in one transaction.
    pub(super) fn remove_queued(&mut self, ids: &[String]) -> Result<(), String> {
        let mut lane_state = self.lane_state();
        if let Some(inbox) = lane_state["inbox"].as_array_mut() {
            inbox.retain(|item| !ids.iter().any(|id| item["entryId"] == id.as_str()));
        }
        let mut writes: Vec<Write> = ids.iter().map(|id| Write::delete(PENDING_ENTRY, id)).collect();
        writes.push(Write::set(LANE_STATE, MAIN_LANE, lane_state));
        lock_session(&self.session).commit(&writes)?;
        if !ids.is_empty() {
            self.withdraw_queued(ids);
        }
        for id in ids {
            let mut queues = sync::lock(&self.queues);
            if let Some(index) = queues.items.iter().position(|q| q.entry_id == *id) {
                queues.items.remove(index);
            }
            drop(queues);
            self.queue_update();
        }
        Ok(())
    }

    pub(super) fn clear_queue(&mut self) -> Value {
        let (steering, follow_up, cleared) = {
            let queues = sync::lock(&self.queues);
            let visible = queues.visible();
            let pick = |kind: QueueKind| -> Vec<String> {
                visible
                    .iter()
                    .filter(|q| q.kind == kind)
                    .map(|q| message_text(&q.message))
                    .collect()
            };
            let cleared: Vec<String> = queues.items.iter().map(|q| q.entry_id.clone()).collect();
            let mut follow_up = pick(QueueKind::FollowUp);
            follow_up.extend(pick(QueueKind::NextRun));
            (pick(QueueKind::Steer), follow_up, cleared)
        };
        if !cleared.is_empty() {
            let _ = self.remove_queued(&cleared);
        }
        json!({"steering": steering, "followUp": follow_up})
    }

    pub(super) fn bash(&mut self, op: Op, reply: Reply) {
        let Op::Bash { command, exclude, id } = op else { return };
        let signal = kernel::Signal::new();
        self.bash = Some(signal.clone());
        let host = self.lane.host.clone();
        let tx = self.tx.clone();
        let prefix = self
            .runtime
            .config
            .settings
            .as_object()
            .and_then(config::keys::shell_command_prefix);
        std::thread::spawn(move || {
            let result = user_bash::run(&host, &command, prefix.as_deref(), &signal, &mut |delta| {
                let _ = tx.send(Input::BashOutput {
                    id: id.clone(),
                    delta: delta.to_string(),
                });
            });
            let _ = tx.send(Input::BashDone {
                command,
                exclude,
                result,
                reply,
            });
        });
    }
}
