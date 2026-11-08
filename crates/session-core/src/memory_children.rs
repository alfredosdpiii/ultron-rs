//! Subagents run as in-memory agent loops (a session without the durable runtime: `ultron mcp`, whose root is
//! Claude Code, with `--children ultron` or no `claude` installed). The same loop as `delegation::child::agent_runner`
//! (the child's reply is its last assistant message's text; cancelling the child aborts the loop), with what a
//! child lane of the session actor gets from its durable conversation:
//!
//! - agent-hub steering ([`MemoryChildren::steer`], the stack's `lane_steer`): a message for a running child is
//!   queued as a steer of its loop, seen after the current turn's tool calls; a child that is not running is not
//!   steered;
//! - the context-pressure reminder (crate::context_reminder): the child lane's context use is the estimate of its
//!   loop's conversation (`estimateContextTokens`: the newest answer's usage plus estimates after it), read while
//!   the child runs.

use crate::context_reminder::ContextReminders;
use agent::{Agent, AgentControl, AgentEvent};
use ai::{Message, UserMessage, UserMessageContent};
use delegation::child::{ChildAgentFactory, run_to_text};
use delegation::host::{ChildRun, ChildRunner};
use jsrt::sync;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

/// The running in-memory children, by lane.
#[derive(Default)]
pub struct MemoryChildren {
    running: Mutex<HashMap<String, AgentControl>>,
    reminders: OnceLock<Arc<ContextReminders>>,
}

impl MemoryChildren {
    /// Child lanes' context use goes to `reminders` from the next child on.
    pub fn attach_reminders(&self, reminders: Arc<ContextReminders>) {
        let _ = self.reminders.set(reminders);
    }

    /// Steer the running child on `lane` with `message`; false when no child runs there.
    pub fn steer(&self, lane: &str, message: &str) -> bool {
        let Some(control) = sync::lock(&self.running).get(lane).cloned() else {
            return false;
        };
        control.steer(Message::User(UserMessage {
            content: UserMessageContent::Text(message.to_string()),
            timestamp: crate::lane::now_ms(),
        }));
        true
    }

    /// Whether a child runs on `lane`.
    pub fn is_running(&self, lane: &str) -> bool {
        sync::lock(&self.running).contains_key(lane)
    }

    /// The runner of subagents built by `factory`.
    pub fn runner(self: &Arc<Self>, factory: ChildAgentFactory) -> ChildRunner {
        let children = self.clone();
        Arc::new(move |run: ChildRun| children.run(&factory, run))
    }

    fn run(&self, factory: &ChildAgentFactory, run: ChildRun) -> Result<String, String> {
        let (options, stream) = factory(&run);
        let mut agent = Agent::new(options, stream);
        // The conversation as the loop has it, for the lane's context meter.
        let conversation = Arc::new(Mutex::new(agent.messages.clone()));
        let seen = conversation.clone();
        agent.subscribe(move |event| {
            if let AgentEvent::MessageEnd { message } = event {
                sync::lock(&seen).push(message.clone());
            }
        });
        let lane = run.lane_name.clone();
        if let Some(reminders) = self.reminders.get() {
            let meter = conversation.clone();
            reminders.set_lane_meter(
                &lane,
                Some(Arc::new(move || {
                    Some(ai::transcript::estimate_context_tokens(&sync::lock(&meter)).tokens)
                })),
            );
        }
        sync::lock(&self.running).insert(lane.clone(), agent.control());
        let reply = run_to_text(&mut agent, &run.prompt, &run.signal);
        sync::lock(&self.running).remove(&lane);
        if let Some(reminders) = self.reminders.get() {
            reminders.set_lane_meter(&lane, None);
        }
        reply
    }
}
