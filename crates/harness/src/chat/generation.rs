//! The built-in generation task `ultron.generation` (spec §8.3, §8.5): prepare the positional system prompt and the REPL
//! tool, request or poll the model with options pinned at preparation, retry with durable backoff, compact once on a
//! context overflow, classify the response, and run the tool round of REPL cells. Port of upstream
//! `harness/generation.ts`; the tool list is exactly the cell executor's REPL tool.

mod tools;

pub use tools::Control;
use tools::{read_calls, start_tool_round, tools_phase};

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, Message, StopReason, ToolCall};
use durable::{
    ConversationId, EntryDraft, EntryId, EntryQuery, JoinPolicy, SubmissionId, SubmissionSettlement, TaskDefinition,
    TaskId, TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskRecord, TaskState, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::Services;
use super::cell::{CellResult, append_tool_result};
use super::compaction::{Threshold, create_compaction, estimate_planned, select_cut, threshold_compaction};
use super::context::read_context;
use super::docs::{AGENT, PROVIDER};
use super::gate::{FINAL_TOOL_CHOICE, RequestGate, gate_run};
use super::inbox::{At, apply_boundary, prepare_boundary, user_entry};
use super::live::{GENERATION_KIND, append_assistant, convert_partial, end_run, hand_over, live};
use super::models::{ModelRef, StreamRequest, is_context_overflow};
use super::prompt::{plan_system_entries, replay_sections};
use super::throttle::Throttle;
use super::util::{delete, from_value, get, owner, set, strict, to_value};
use crate::scheduler::{HarnessError, NextState, Result, Task, TaskRuntime, completed};

const DEFAULT_POLL_AFTER_MS: u64 = 5000;

/// The generation's checkpoint (spec §8.3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub(crate) enum GenerationCheckpoint {
    Prepare {
        attempt: u64,
        /// The blocking compaction this generation waited for; it starts no other compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        /// Error text of the overflow that started `compacted`; checked once when `prepare` resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        overflow: Option<String>,
    },
    Request {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        thinking_level: String,
        /// The settings' request options when preparation committed; a resend after recovery uses them unchanged.
        stream_options: Map<String, Value>,
        /// Newest entry included in the request.
        cutoff: EntryId,
    },
    Retry {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        until: u64,
    },
    Poll {
        attempt: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        compacted: Option<TaskId>,
        model: ModelRef,
        cutoff: EntryId,
        handle: ai::DeferredHandle,
        poll_at: u64,
    },
    /// Waiting on the round's cell tasks, which the generation owns.
    Tools {
        /// The tool-calling answer.
        assistant: EntryId,
        /// Cell tasks created so far, in call order.
        tools: Vec<TaskId>,
        /// Calls of a sequential round not started yet, in call order.
        pending: Vec<String>,
    },
}

fn checkpoint(value: GenerationCheckpoint) -> Value {
    to_value(&value)
}

/// The Session-facing definition: version 1, starting at `{ phase: "prepare", attempt: 1 }`.
pub(crate) static GENERATION_DEFINITION: LazyLock<TaskDefinition> = LazyLock::new(|| {
    TaskDefinition::new(GENERATION_KIND, 1, |_| {
        checkpoint(GenerationCheckpoint::Prepare {
            attempt: 1,
            compacted: None,
            overflow: None,
        })
    })
});

/// A generation owned by its conversation.
pub(crate) fn create_generation(tx: &mut Tx<'_>, conversation_id: ConversationId) -> Result<TaskId> {
    Ok(tx.create_task(
        &GENERATION_DEFINITION,
        json!({}),
        TaskOptions {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation_id),
            ..TaskOptions::default()
        },
    )?)
}

/// Start a run for `inputs`, placed input submissions: a new generation takes `ultron.live.run`.
pub(crate) fn start_run(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    live: &chord_delta::Draft,
    inputs: Vec<SubmissionId>,
) -> Result<()> {
    let task_id = create_generation(tx, conversation_id)?;
    let inputs: Vec<u64> = inputs.into_iter().map(|id| id.0).collect();
    set(live, &["run"], json!({ "taskId": task_id.0, "inputs": inputs }))
}

/// The agent a phase resolves: the conversation's model and thinking level.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Agent {
    pub model: Option<ModelRef>,
    pub thinking_level: String,
}

pub(crate) fn read_agent(runtime: &TaskRuntime) -> Result<Agent> {
    let state = runtime.snapshot(&AGENT, owner(runtime.conversation_id()))?;
    let state = state.map(|state| (*state).clone()).unwrap_or(json!({}));
    let model = match state.get("model") {
        Some(model) if !model.is_null() => Some(from_value::<ModelRef>(model.clone(), "ultron.agent model")?),
        _ => None,
    };
    Ok(Agent {
        model,
        thinking_level: state["thinkingLevel"].as_str().unwrap_or("off").to_owned(),
    })
}

/// The persisted provider session ID; a legacy conversation without `ultron.provider` gets one in its own commit.
pub(crate) fn ensure_provider_session_id(runtime: &TaskRuntime) -> Result<String> {
    let conversation_id = runtime.conversation_id();
    if let Some(state) = runtime.snapshot(&PROVIDER, owner(conversation_id))? {
        return Ok(state["sessionId"].as_str().unwrap_or_default().to_owned());
    }
    let created = Arc::new(Mutex::new(String::new()));
    let slot = created.clone();
    runtime.commit(&runtime.ctx(), move |tx, _| {
        let provider = tx.doc(&PROVIDER, owner(conversation_id))?;
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = get(&provider, &["sessionId"])?
            .and_then(|id| id.as_str().map(str::to_owned))
            .unwrap_or_default();
        Ok(None)
    })?;
    let id = created
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok(id)
}

fn running(value: GenerationCheckpoint) -> Option<NextState> {
    Some(NextState::Running {
        checkpoint: checkpoint(value),
    })
}

fn failed(message: &str, reason: &str) -> Option<NextState> {
    Some(NextState::Terminal {
        outcome: TaskOutcome::Failed {
            error: TaskOutcomeError {
                message: message.to_owned(),
                detail: Some(json!({ "reason": reason })),
            },
            result: None,
        },
    })
}

pub(crate) fn generation_task(services: Arc<Services>) -> Task {
    let (prepare, request, retry, poll, tools, abort) = (
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services.clone(),
        services,
    );
    Task::define(GENERATION_KIND, 1, |input| (GENERATION_DEFINITION.initial)(input))
        .phase("prepare", move |task, runtime| prepare_phase(&prepare, task, runtime))
        .phase("request", move |task, runtime| request_phase(&request, task, runtime))
        .phase("retry", move |task, runtime| retry_phase(&retry, task, runtime))
        .phase("poll", move |task, runtime| poll_phase(&poll, task, runtime))
        .phase("tools", move |task, runtime| tools_phase(&tools, task, runtime))
        .abort(move |task, runtime| abort_handler(&abort, task, runtime))
        .build()
}

pub(crate) fn current_checkpoint(task: &TaskRecord) -> Result<GenerationCheckpoint> {
    let value = match &task.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => checkpoint.clone(),
        _ => return Err(HarnessError::msg("A terminal generation has no checkpoint")),
    };
    from_value(value, "generation checkpoint")
}

/// Settle the run's inputs `unanswered` with `model_error` and fail with `text`.
fn fail_model_error(runtime: &TaskRuntime, text: &str) -> Result<()> {
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "model_error".into(),
                detail: Some(json!(text)),
            },
        )?;
        Ok(failed(text, "model_error"))
    })
}

/// A request the gate refused (`request_blocked`): the run's inputs settle `unanswered` with the reason, no answer
/// is appended, and the run fails.
fn fail_blocked(runtime: &TaskRuntime, reason: &str) -> Result<()> {
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "request_blocked".into(),
                detail: Some(json!(reason)),
            },
        )?;
        Ok(failed(reason, "request_blocked"))
    })
}

/// Settle the run's inputs `unanswered` with `no_model` and fail.
fn fail_no_model(runtime: &TaskRuntime, model: Option<&ModelRef>) -> Result<()> {
    let message = match model {
        None => "No model is configured".to_owned(),
        Some(model) => format!("Model {}/{} is not available", model.provider, model.model_id),
    };
    let task_id = runtime.task_id();
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "no_model".into(),
                detail: None,
            },
        )?;
        Ok(failed(&message, "no_model"))
    })
}

fn overflow_compaction_succeeded(runtime: &TaskRuntime, compacted: TaskId) -> Result<bool> {
    let outcomes = runtime.outcomes(&[compacted])?;
    Ok(matches!(
        outcomes.first(),
        Some(TaskOutcome::Completed { result }) if result.get("entryId").is_some()
    ))
}

fn resolve_desired_sections(
    services: &Services,
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    shown: indexmap::IndexMap<String, String>,
) -> Result<indexmap::IndexMap<String, String>> {
    let Some(sections) = &services.sections else {
        return Ok(Default::default());
    };
    match sections(conversation_id, runtime.signal()) {
        Ok(desired) => Ok(desired),
        Err(error) => {
            runtime.signal().check()?;
            runtime.report(&error)?;
            Ok(shown)
        }
    }
}

fn apply_before_compaction(
    services: &Services,
    conversation_id: ConversationId,
    view: &super::context::ContextView,
    planned: &[EntryDraft],
    context_window: u64,
    policy: &super::models::CompactionPolicy,
    threshold: Option<Threshold>,
) -> Result<(Option<Threshold>, Vec<EntryDraft>)> {
    if threshold != Some(Threshold::Blocking) {
        return Ok((threshold, Vec::new()));
    }
    let Some(hook) = &services.hooks.before_compaction else {
        return Ok((threshold, Vec::new()));
    };
    let tokens = estimate_planned(view, planned)?;
    let limit = context_window.saturating_sub(policy.reserve_tokens);
    let mut declined = Vec::new();
    for entry in hook(conversation_id, tokens, limit) {
        declined.push(super::inbox::entry_draft(&entry)?);
    }
    let next_threshold = if declined.is_empty() { threshold } else { None };
    Ok((next_threshold, declined))
}

fn commit_blocking_compaction(runtime: &TaskRuntime, conversation_id: ConversationId, attempt: u64) -> Result<()> {
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let child = create_compaction(tx, conversation_id, "threshold", None, Some(task_id))?;
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Prepare {
                attempt,
                compacted: Some(child),
                overflow: None,
            }),
            on: vec![child],
            policy: JoinPolicy::AllSettled,
        }))
    })
}

#[allow(clippy::too_many_arguments, reason = "commit prepared generation checkpoint")]
fn commit_prepared_request(
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    declined: Vec<EntryDraft>,
    planned: Vec<EntryDraft>,
    threshold: Option<Threshold>,
    attempt: u64,
    compacted: Option<TaskId>,
    model_ref: ModelRef,
    thinking_level: String,
    stream_options: Map<String, Value>,
) -> Result<()> {
    runtime.commit(&runtime.ctx(), |tx, _| {
        let query = EntryQuery {
            conversation_id,
            min_entry_id: None,
            max_entry_id: None,
            order: None,
        };
        let mut cutoff = tx.scan_entries(&query, 1, None)?.items.first().map(|entry| entry.id);
        for entry in declined.into_iter().chain(planned) {
            cutoff = Some(tx.append_entry(conversation_id, entry)?.id);
        }
        let Some(cutoff) = cutoff else {
            return Err(HarnessError::Error(format!(
                "Conversation {} has no entries to send",
                conversation_id.0
            )));
        };
        if threshold == Some(Threshold::Background) && get(&live(tx, conversation_id)?, &["compactions"])?.is_none() {
            create_compaction(tx, conversation_id, "threshold", None, None)?;
        }
        Ok(running(GenerationCheckpoint::Request {
            attempt,
            compacted,
            model: model_ref.clone(),
            thinking_level: thinking_level.clone(),
            stream_options: stream_options.clone(),
            cutoff,
        }))
    })
}

fn resolve_prepare_model(services: &Services, runtime: &TaskRuntime) -> Result<Option<(ModelRef, ai::Model, String)>> {
    let agent = read_agent(runtime)?;
    let Some(model_ref) = agent.model.clone() else {
        fail_no_model(runtime, None)?;
        return Ok(None);
    };
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        fail_no_model(runtime, Some(&model_ref))?;
        return Ok(None);
    };
    Ok(Some((model_ref, model, agent.thinking_level)))
}

fn plan_prepare_entries(
    services: &Services,
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    context_window: u64,
    compacted: Option<TaskId>,
    settings: &super::models::Settings,
) -> Result<(Option<Threshold>, Vec<EntryDraft>, Vec<EntryDraft>)> {
    let view = runtime.read(|tx| read_context(tx, conversation_id, None))?;
    let desired = resolve_desired_sections(services, runtime, conversation_id, replay_sections(&view.messages))?;
    let planned = plan_system_entries(&view, &desired, &[services.cells.tool()], runtime.now()?);
    let policy = services.compaction_policy(conversation_id, settings);
    let threshold = match compacted {
        Some(_) => None,
        None => threshold_compaction(&view, &planned, context_window, &policy)?,
    };
    let (threshold, declined) = apply_before_compaction(
        services,
        conversation_id,
        &view,
        &planned,
        context_window,
        &policy,
        threshold,
    )?;
    Ok((threshold, declined, planned))
}

fn prepare_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Prepare {
        attempt,
        compacted,
        overflow,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("prepare phase without a prepare checkpoint"));
    };
    let conversation_id = runtime.conversation_id();
    let Some((model_ref, model, thinking_level)) = resolve_prepare_model(services, runtime)? else {
        return Ok(());
    };
    if let (Some(compacted), Some(overflow)) = (compacted, overflow.as_deref())
        && !overflow_compaction_succeeded(runtime, compacted)?
    {
        return fail_model_error(runtime, overflow);
    }
    let settings = (services.settings)();
    let (threshold, declined, planned) = plan_prepare_entries(
        services,
        runtime,
        conversation_id,
        model.context_window,
        compacted,
        &settings,
    )?;
    if threshold == Some(Threshold::Blocking) {
        return commit_blocking_compaction(runtime, conversation_id, attempt);
    }
    commit_prepared_request(
        runtime,
        conversation_id,
        declined,
        planned,
        threshold,
        attempt,
        compacted,
        model_ref,
        thinking_level,
        settings.stream,
    )
}

/// What classification needs from the request that produced a message.
struct Request {
    attempt: u64,
    compacted: Option<TaskId>,
    model: ModelRef,
    cutoff: EntryId,
    /// Committed model context through `cutoff`, when the phase already derived it.
    messages: Option<Vec<Message>>,
    /// Set when the message came from polling, so a still deferred result polls strictly later.
    poll_at: Option<u64>,
}

fn begin_request_attempt(runtime: &TaskRuntime, conversation_id: ConversationId, attempt: u64) -> Result<()> {
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        convert_partial(tx, &live, conversation_id)?;
        set(&live, &["generation"], json!({ "attempt": attempt }))?;
        Ok(None)
    })
}

fn apply_request_gate(
    services: &Services,
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    stream_options: &mut Map<String, Value>,
) -> Result<Option<Option<String>>> {
    let Some(gate) = &services.hooks.gate_request else {
        return Ok(Some(None));
    };
    let run = runtime.read(|tx| gate_run(tx, conversation_id))?;
    match gate(&run) {
        RequestGate::Allow => Ok(Some(None)),
        RequestGate::Final { note } => {
            stream_options.insert("toolChoice".into(), json!(FINAL_TOOL_CHOICE));
            Ok(Some(Some(note)))
        }
        RequestGate::Block { reason } => {
            fail_blocked(runtime, &reason)?;
            Ok(None)
        }
    }
}

fn request_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Request {
        attempt,
        compacted,
        model: model_ref,
        thinking_level,
        mut stream_options,
        cutoff,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("request phase without a request checkpoint"));
    };
    let conversation_id = runtime.conversation_id();
    begin_request_attempt(runtime, conversation_id, attempt)?;
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(runtime, Some(&model_ref));
    };
    let view = runtime.read(|tx| read_context(tx, conversation_id, Some(cutoff)))?;
    let messages = match &services.hooks.before_request {
        Some(hook) => hook(view.messages.clone()),
        None => view.messages.clone(),
    };
    let Some(final_note) = apply_request_gate(services, runtime, conversation_id, &mut stream_options)? else {
        return Ok(());
    };
    let request = StreamRequest {
        options: stream_options,
        session_id: Some(ensure_provider_session_id(runtime)?),
        reasoning: (thinking_level != "off").then_some(thinking_level),
        conversation_id: Some(conversation_id),
        final_note,
    };
    let message = stream_response(services, runtime, &model, messages, &request, attempt)?;
    let request = Request {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        messages: Some(view.messages),
        poll_at: None,
    };
    classify(services, runtime, request, message)
}

fn commit_stream_partial(committer: &TaskRuntime, attempt: u64, partial: Value) {
    let conversation_id = committer.conversation_id();
    let result = committer.commit(&committer.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        if get(&live, &["generation"])?.is_none() {
            set(&live, &["generation"], json!({ "attempt": attempt }))?;
        }
        set(&live, &["generation", "message"], partial)?;
        Ok(None)
    });
    if let Err(error) = result
        && !committer.signal().is_cancelled()
    {
        let _ = committer.report(&error);
    }
}

/// Stream one request and return the terminal message. Partials commit as throttled writes with one commit in
/// flight; the throttle stops and awaits that commit before returning, so no stale partial lands after the outcome.
fn stream_response(
    services: &Services,
    runtime: &TaskRuntime,
    model: &ai::Model,
    messages: Vec<Message>,
    request: &StreamRequest,
    attempt: u64,
) -> Result<AssistantMessage> {
    let interval = Duration::from_millis((services.settings)().partial_interval_ms);
    let committer = runtime.clone();
    let throttle = Throttle::new(interval, move |partial: Value| {
        commit_stream_partial(&committer, attempt, partial);
    });
    let context = ai::TranscriptContext { messages };
    let mut on_event = |event: &AssistantMessageEvent| {
        if let Some(partial) = event.partial()
            && !partial.content.is_empty()
        {
            throttle.mark(strict(to_value(partial)));
        }
    };
    let result = services
        .models
        .stream(model, &context, request, runtime.signal(), &mut on_event);
    throttle.stop();
    runtime.signal().check()?;
    result
}

fn tool_calls(message: &AssistantMessage) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn stop_reason_name(reason: StopReason) -> String {
    to_value(&reason).as_str().unwrap_or_default().to_owned()
}

fn schedule_deferred_poll(runtime: &TaskRuntime, request: Request, handle: ai::DeferredHandle) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let after = handle
        .poll_after_ms
        .and_then(|ms| u64::try_from(ms).ok())
        .unwrap_or(DEFAULT_POLL_AFTER_MS);
    let poll_at = (runtime.now()? + after).max(request.poll_at.map_or(0, |previous| previous + 1));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        set(
            &live,
            &["generation"],
            json!({ "attempt": request.attempt, "deferred": { "pollAt": poll_at } }),
        )?;
        Ok(running(GenerationCheckpoint::Poll {
            attempt: request.attempt,
            compacted: request.compacted,
            model: request.model.clone(),
            cutoff: request.cutoff,
            handle: handle.clone(),
            poll_at,
        }))
    })
}

fn commit_overflow_compaction(runtime: &TaskRuntime, attempt: u64, stored: &Value, text: String) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        append_assistant(tx, conversation_id, stored)?;
        delete(&live, &["generation"])?;
        let child = create_compaction(tx, conversation_id, "overflow", None, Some(task_id))?;
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Prepare {
                attempt,
                compacted: Some(child),
                overflow: Some(text.clone()),
            }),
            on: vec![child],
            policy: JoinPolicy::AllSettled,
        }))
    })
}

fn try_overflow_compaction(
    services: &Services,
    runtime: &TaskRuntime,
    request: &Request,
    message: &AssistantMessage,
    stored: &Value,
    settings: &super::models::Settings,
) -> Result<bool> {
    let conversation_id = runtime.conversation_id();
    let policy = services.compaction_policy(conversation_id, settings);
    if request.compacted.is_some() || !policy.enabled {
        return Ok(false);
    }
    let view = runtime.read(|tx| read_context(tx, conversation_id, Some(request.cutoff)))?;
    if select_cut(&view, policy.keep_recent_tokens).is_none() {
        return Ok(false);
    }
    let text = message
        .error_message
        .clone()
        .unwrap_or_else(|| "Context overflow".into());
    commit_overflow_compaction(runtime, request.attempt, stored, text)?;
    Ok(true)
}

fn commit_error_or_retry(
    runtime: &TaskRuntime,
    request: &Request,
    message: &AssistantMessage,
    stored: &Value,
    retry: Option<u64>,
) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        append_assistant(tx, conversation_id, stored)?;
        if let Some(until) = retry {
            let error = message.error_message.clone().unwrap_or_default();
            set(
                &live,
                &["generation"],
                json!({ "attempt": request.attempt, "retry": { "at": until, "error": error } }),
            )?;
            return Ok(running(GenerationCheckpoint::Retry {
                attempt: request.attempt,
                compacted: request.compacted,
                until,
            }));
        }
        let text = message.error_message.clone().unwrap_or_else(|| {
            format!(
                "Model response ended with stop reason {}",
                stop_reason_name(message.stop_reason)
            )
        });
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "model_error".into(),
                detail: Some(json!(text)),
            },
        )?;
        Ok(failed(&text, "model_error"))
    })
}

fn classify_error_response(
    services: &Services,
    runtime: &TaskRuntime,
    request: &Request,
    message: &AssistantMessage,
) -> Result<()> {
    let settings = (services.settings)();
    let overflow = message.stop_reason == StopReason::Error && is_context_overflow(message);
    let stored = strict(to_value(message));
    if overflow && try_overflow_compaction(services, runtime, request, message, &stored, &settings)? {
        return Ok(());
    }
    let retry = settings
        .retry
        .retry_delay(request.attempt, message)
        .filter(|_| !overflow)
        .map(|delay| runtime.now().map(|now| now + delay))
        .transpose()?;
    commit_error_or_retry(runtime, request, message, &stored, retry)
}

/// Classify a terminal provider message in one commit that also clears the partial.
fn classify(services: &Services, runtime: &TaskRuntime, request: Request, message: AssistantMessage) -> Result<()> {
    runtime.signal().check()?;
    if message.stop_reason == StopReason::Deferred
        && let Some(handle) = message.deferred.clone()
    {
        return schedule_deferred_poll(runtime, request, handle);
    }
    if let Some(hook) = &services.hooks.after_response {
        hook(&message);
    }
    let calls = tool_calls(&message);
    if message.stop_reason == StopReason::ToolUse && !calls.is_empty() {
        return start_tool_round(services, runtime, request, message, calls);
    }
    if matches!(
        message.stop_reason,
        StopReason::Stop | StopReason::Length | StopReason::ToolUse
    ) {
        return answer(services, runtime, message);
    }
    classify_error_response(services, runtime, &request, &message)
}

fn continue_yield_run(
    tx: &mut Tx<'_>,
    live: &chord_delta::Draft,
    conversation_id: ConversationId,
    task_id: TaskId,
    content: &Value,
    now: u64,
) -> Result<()> {
    tx.append_entry(conversation_id, user_entry(content, now))?;
    let successor = create_generation(tx, conversation_id)?;
    hand_over(live, task_id, successor)?;
    delete(live, &["generation"])
}

fn finish_answer_run(
    tx: &mut Tx<'_>,
    live: &chord_delta::Draft,
    conversation_id: ConversationId,
    task_id: TaskId,
    answer: EntryId,
    users: Vec<SubmissionId>,
) -> Result<()> {
    end_run(tx, live, task_id, SubmissionSettlement::Done { answer })?;
    if !users.is_empty() {
        start_run(tx, conversation_id, live, users)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "answer boundary settlement")]
fn settle_answer_boundary(
    tx: &mut Tx<'_>,
    boundary: &mut super::inbox::Boundary,
    live: &chord_delta::Draft,
    conversation_id: ConversationId,
    task_id: TaskId,
    answer: EntryId,
    continuation: Option<&Value>,
    now: u64,
) -> Result<()> {
    let placed = apply_boundary(tx, boundary, At::Final, now)?;
    if let Some(content) = continuation
        && placed.users.is_empty()
        && !placed.reset
    {
        continue_yield_run(tx, live, conversation_id, task_id, content, now)
    } else {
        finish_answer_run(tx, live, conversation_id, task_id, answer, placed.users)
    }
}

/// A final answer; the final boundary places queued items (spec §6). The first `on_yield` continuation appends a
/// user message and hands the run to a successor generation, but only when the boundary selected no user item and no
/// reset. Otherwise the run's inputs settle `done`, and selected user items start the next run.
fn answer(services: &Services, runtime: &TaskRuntime, message: AssistantMessage) -> Result<()> {
    let continuation = services.hooks.on_yield.as_ref().and_then(|hook| hook(&message));
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let stored = strict(to_value(&message));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let mut boundary = prepare_boundary(tx, conversation_id, (services.settings)().queue_modes())?;
        let live = live(tx, conversation_id)?;
        let entry = append_assistant(tx, conversation_id, &stored)?;
        let now = runtime.now()?;
        settle_answer_boundary(
            tx,
            &mut boundary,
            &live,
            conversation_id,
            task_id,
            entry.id,
            continuation.as_ref(),
            now,
        )?;
        Ok(Some(completed(json!({ "entryId": entry.id.0 }))))
    })
}

fn retry_phase(_services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Retry {
        attempt,
        compacted,
        until,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("retry phase without a retry checkpoint"));
    };
    runtime.sleep(until, &runtime.ctx())?;
    let conversation_id = runtime.conversation_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        set(&live, &["generation"], json!({ "attempt": attempt + 1 }))?;
        Ok(running(GenerationCheckpoint::Prepare {
            attempt: attempt + 1,
            compacted,
            overflow: None,
        }))
    })
}

fn poll_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Poll {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        handle,
        poll_at,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("poll phase without a poll checkpoint"));
    };
    let Some(model) = services.models.get_model(&model_ref.provider, &model_ref.model_id) else {
        return fail_no_model(runtime, Some(&model_ref));
    };
    runtime.sleep(poll_at, &runtime.ctx())?;
    let message = services.models.fetch_deferred(&model, &handle, runtime.signal())?;
    let request = Request {
        attempt,
        compacted,
        model: model_ref,
        cutoff,
        messages: None,
        poll_at: Some(poll_at),
    };
    classify(services, runtime, request, message)
}

fn cancel_pending_deferred(
    services: &Services,
    runtime: &TaskRuntime,
    checkpoint: &GenerationCheckpoint,
) -> Result<()> {
    if let GenerationCheckpoint::Poll { model, handle, .. } = checkpoint
        && let Some(model) = services.models.get_model(&model.provider, &model.model_id)
        && let Err(error) = services.models.cancel_deferred(&model, handle, runtime.signal())
    {
        runtime.report(&error)?;
    }
    Ok(())
}

fn append_aborted_calls(
    tx: &mut Tx<'_>,
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    unstarted: &[ToolCall],
) -> Result<()> {
    for call in unstarted {
        let aborted = CellResult::error("aborted", &format!("Tool {} was aborted", call.name));
        append_tool_result(
            tx,
            conversation_id,
            &call.id,
            &call.name,
            &aborted,
            runtime.now()?,
            None,
        )?;
    }
    Ok(())
}

/// Runs after the round's cells are terminal: calls never started get `aborted` results; the run's inputs settle
/// `unanswered` with `aborted`.
fn abort_handler(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let checkpoint = current_checkpoint(task)?;
    cancel_pending_deferred(services, runtime, &checkpoint)?;
    let unstarted = match &checkpoint {
        GenerationCheckpoint::Tools { assistant, pending, .. } => read_calls(runtime, *assistant, pending)?,
        _ => Vec::new(),
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        convert_partial(tx, &live, conversation_id)?;
        append_aborted_calls(tx, runtime, conversation_id, &unstarted)?;
        end_run(
            tx,
            &live,
            task_id,
            SubmissionSettlement::Unanswered {
                reason: "aborted".into(),
                detail: None,
            },
        )?;
        Ok(Some(NextState::Terminal {
            outcome: TaskOutcome::Aborted {
                reason: None,
                result: None,
            },
        }))
    })
}
