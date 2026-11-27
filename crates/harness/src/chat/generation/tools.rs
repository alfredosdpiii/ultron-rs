//! Tool-round execution and settlement for `ultron.generation`.

use ai::{AssistantMessage, Message, ToolCall};
use durable::{
    ConversationId, EntryDraft, EntryHead, EntryId, JoinPolicy, SubmissionId, SubmissionSettlement, TaskId,
    TaskOptions, TaskOutcome, TaskOwnership, TaskRecord, Tx,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::Services;
use super::super::cell::{CellResult, append_tool_result, cell_definition};
use super::super::context::read_context;
use super::super::docs::LIVE;
use super::super::entries;
use super::super::inbox::{At, apply_boundary, prepare_boundary};
use super::super::live::{append_assistant, end_run, hand_over, live, run_task};
use super::super::util::{delete, get, owner, set, strict, to_value};
use super::{GenerationCheckpoint, Request, checkpoint, create_generation, current_checkpoint, start_run, tool_calls};
use crate::scheduler::{HarnessError, NextState, Result, TaskRuntime, completed};

/// A cell task for call `call_id`, owned by the generation.
fn create_cell_task(tx: &mut Tx<'_>, generation: TaskId, assistant: EntryId, call_id: &str) -> Result<TaskId> {
    Ok(tx.create_task(
        cell_definition(),
        json!({ "kind": "model", "assistant": assistant.0, "callId": call_id }),
        TaskOptions {
            ownership: TaskOwnership::Task(generation),
            ..TaskOptions::default()
        },
    )?)
}

fn offered_tool_names(runtime: &TaskRuntime, request: Request) -> Result<Vec<String>> {
    let conversation_id = runtime.conversation_id();
    let messages = match request.messages {
        Some(messages) => messages,
        None => {
            runtime
                .read(|tx| read_context(tx, conversation_id, Some(request.cutoff)))?
                .messages
        }
    };
    Ok(ai::transcript::current_tools(&messages)
        .into_iter()
        .map(|tool| tool.name)
        .collect())
}

#[allow(clippy::too_many_arguments, reason = "tool-round enqueue helper")]
fn enqueue_tool_call(
    tx: &mut Tx<'_>,
    runtime: &TaskRuntime,
    conversation_id: ConversationId,
    task_id: TaskId,
    assistant_entry: EntryId,
    call: &ToolCall,
    offered: &[String],
    sequential: bool,
    tools: &mut Vec<TaskId>,
    pending: &mut Vec<String>,
) -> Result<Value> {
    if !offered.contains(&call.name) {
        let unavailable = CellResult::error("tool_unavailable", &format!("Tool {} is not available", call.name));
        let result = append_tool_result(
            tx,
            conversation_id,
            &call.id,
            &call.name,
            &unavailable,
            runtime.now()?,
            None,
        )?;
        return Ok(json!({ "callId": call.id, "name": call.name, "status": "done", "entry": result.id.0 }));
    }
    if sequential && !tools.is_empty() {
        pending.push(call.id.clone());
        return Ok(json!({ "callId": call.id, "name": call.name, "status": "pending" }));
    }
    let cell = create_cell_task(tx, task_id, assistant_entry, &call.id)?;
    tools.push(cell);
    Ok(json!({ "callId": call.id, "name": call.name, "taskId": cell.0, "status": "pending" }))
}

/// Append the tool-calling answer and start its tool round in one commit (spec §8.3). A call to a tool the request did
/// not offer gets its `tool_unavailable` result here; every other call gets a cell task owned by the generation, only
/// the first one now when the round is sequential. The generation then waits for them in its `tools` phase.
pub(super) fn start_tool_round(
    services: &Services,
    runtime: &TaskRuntime,
    request: Request,
    message: AssistantMessage,
    calls: Vec<ToolCall>,
) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let offered = offered_tool_names(runtime, request)?;
    let sequential = (services.settings)().tool_execution == super::super::models::ToolExecution::Sequential;
    let stored = strict(to_value(&message));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        let entry = append_assistant(tx, conversation_id, &stored)?;
        let mut slots = Vec::new();
        let mut tools = Vec::new();
        let mut pending = Vec::new();
        for call in &calls {
            let slot = enqueue_tool_call(
                tx,
                runtime,
                conversation_id,
                task_id,
                entry.id,
                call,
                &offered,
                sequential,
                &mut tools,
                &mut pending,
            )?;
            slots.push(slot);
        }
        delete(&live, &["generation"])?;
        set(&live, &["tools"], Value::Array(slots))?;
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Tools {
                assistant: entry.id,
                tools: tools.clone(),
                pending,
            }),
            on: tools,
            policy: JoinPolicy::AllSettled,
        }))
    })
}

pub(super) fn tools_phase(services: &Services, task: &TaskRecord, runtime: &TaskRuntime) -> Result<()> {
    let GenerationCheckpoint::Tools {
        assistant,
        tools,
        pending,
    } = current_checkpoint(task)?
    else {
        return Err(HarnessError::msg("tools phase without a tools checkpoint"));
    };
    let Some((next, rest)) = pending.split_first() else {
        return finish_tool_round(services, runtime, assistant, tools);
    };
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    runtime.commit(&runtime.ctx(), |tx, _| {
        let live = live(tx, conversation_id)?;
        let cell = create_cell_task(tx, task_id, assistant, next)?;
        let slots = get(&live, &["tools"])?.unwrap_or(json!([]));
        if let Some(index) = slots.as_array().and_then(|slots| {
            slots
                .iter()
                .position(|slot| slot["callId"] == json!(next) && slot.get("taskId").is_none())
        }) {
            super::super::util::d(live.set(&super::super::live::slot_path(index, Some("taskId")), json!(cell.0)))?;
        }
        let mut started = tools.clone();
        started.push(cell);
        Ok(Some(NextState::Waiting {
            checkpoint: checkpoint(GenerationCheckpoint::Tools {
                assistant,
                tools: started,
                pending: rest.to_vec(),
            }),
            on: vec![cell],
            policy: JoinPolicy::AllSettled,
        }))
    })
}

/// The control a cell asked for: end the run (`terminate`), or start a fresh context with `handoff` text.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Control {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<String>,
}

fn extract_tool_controls(tools: &[TaskId], outcomes: Vec<TaskOutcome>) -> Vec<(TaskId, Option<Control>)> {
    tools
        .iter()
        .zip(outcomes)
        .map(|(id, outcome)| {
            let control = match outcome {
                TaskOutcome::Completed { result } if result["kind"] != "nested" => result
                    .get("control")
                    .and_then(|control| serde_json::from_value(control.clone()).ok()),
                _ => None,
            };
            (*id, control)
        })
        .collect()
}

fn should_terminate_round(slots: &[Value], controls: &[(TaskId, Option<Control>)]) -> bool {
    let control_of = |id: TaskId| {
        controls
            .iter()
            .find(|(each, _)| *each == id)
            .and_then(|(_, control)| control.as_ref())
    };
    !slots.is_empty()
        && slots.iter().all(|slot| {
            slot["taskId"]
                .as_u64()
                .is_some_and(|id| control_of(TaskId(id)).is_some_and(|control| control.terminate == Some(true)))
        })
}

fn finish_round_terminal(
    tx: &mut Tx<'_>,
    boundary: &mut super::super::inbox::Boundary,
    live: &chord_delta::Draft,
    task_id: TaskId,
    assistant: EntryId,
    handoff: Option<&str>,
    now: u64,
) -> Result<()> {
    let conversation_id = boundary.conversation_id;
    if let Some(text) = handoff {
        let entry = tx.append_entry(
            conversation_id,
            EntryDraft {
                head: Some(EntryHead::SelfId),
                model: Some(vec![json!({ "role": "user", "content": text, "timestamp": now })]),
                ..EntryDraft::new(entries::RESET)
            },
        )?;
        boundary.head = Some(entry.id);
    }
    let placed = apply_boundary(tx, boundary, At::Final, now)?;
    end_run(tx, live, task_id, SubmissionSettlement::Done { answer: assistant })?;
    if !placed.users.is_empty() {
        start_run(tx, conversation_id, live, placed.users)?;
    }
    Ok(())
}

fn end_reset_round(
    tx: &mut Tx<'_>,
    live: &chord_delta::Draft,
    conversation_id: ConversationId,
    task_id: TaskId,
    users: Vec<SubmissionId>,
) -> Result<()> {
    end_run(
        tx,
        live,
        task_id,
        SubmissionSettlement::Unanswered {
            reason: "reset".into(),
            detail: None,
        },
    )?;
    if !users.is_empty() {
        start_run(tx, conversation_id, live, users)?;
    }
    Ok(())
}

fn append_run_inputs(live: &chord_delta::Draft, task_id: TaskId, users: &[SubmissionId]) -> Result<()> {
    if run_task(live)? == Some(task_id) {
        for user in users {
            super::super::util::d(live.push(&super::super::util::keys(&["run", "inputs"]), vec![json!(user.0)]))?;
        }
    }
    Ok(())
}

fn finish_round_continue(
    tx: &mut Tx<'_>,
    boundary: &mut super::super::inbox::Boundary,
    live: &chord_delta::Draft,
    task_id: TaskId,
    now: u64,
) -> Result<()> {
    let conversation_id = boundary.conversation_id;
    let placed = apply_boundary(tx, boundary, At::PostTools, now)?;
    if placed.reset {
        return end_reset_round(tx, live, conversation_id, task_id, placed.users);
    }
    delete(live, &["tools"])?;
    delete(live, &["nestedTools"])?;
    append_run_inputs(live, task_id, &placed.users)?;
    let successor = create_generation(tx, conversation_id)?;
    hand_over(live, task_id, successor)
}

/// The round's cells are terminal: apply their controls and either end the run at the final boundary (`terminate`,
/// `handoff`, or a queued reset) or hand it to the next generation at the `postTools` boundary (spec §8.5).
fn finish_tool_round(services: &Services, runtime: &TaskRuntime, assistant: EntryId, tools: Vec<TaskId>) -> Result<()> {
    let conversation_id = runtime.conversation_id();
    let task_id = runtime.task_id();
    let controls = extract_tool_controls(&tools, runtime.outcomes(&tools)?);
    let slots = runtime
        .snapshot(&LIVE, owner(conversation_id))?
        .and_then(|live| live.get("tools").cloned())
        .unwrap_or(json!([]));
    let slots = slots.as_array().cloned().unwrap_or_default();
    let terminate = should_terminate_round(&slots, &controls);
    let handoff = controls
        .iter()
        .rev()
        .find_map(|(_, control)| control.as_ref().and_then(|control| control.handoff.clone()));
    runtime.commit(&runtime.ctx(), |tx, _| {
        let mut boundary = prepare_boundary(tx, conversation_id, (services.settings)().queue_modes())?;
        let live = live(tx, conversation_id)?;
        let now = runtime.now()?;
        if terminate || handoff.is_some() {
            finish_round_terminal(tx, &mut boundary, &live, task_id, assistant, handoff.as_deref(), now)?;
        } else {
            finish_round_continue(tx, &mut boundary, &live, task_id, now)?;
        }
        Ok(Some(completed(json!({ "entryId": assistant.0 }))))
    })
}

/// The calls `call_ids` of the assistant entry, in the given order.
pub(super) fn read_calls(runtime: &TaskRuntime, assistant: EntryId, call_ids: &[String]) -> Result<Vec<ToolCall>> {
    let entry = runtime.read(|tx| Ok(tx.entry_of(entries::ASSISTANT, assistant)?))?;
    let calls = match entry
        .and_then(|entry| entry.model)
        .and_then(|model| model.into_iter().next())
    {
        Some(message) => match super::super::context::message_of(message)? {
            Message::Assistant(message) => tool_calls(&message),
            _ => Vec::new(),
        },
        None => Vec::new(),
    };
    Ok(call_ids
        .iter()
        .filter_map(|id| calls.iter().find(|call| &call.id == id).cloned())
        .collect())
}
