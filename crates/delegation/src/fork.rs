//! What a subagent starts with (`rlm.spawn(context=...)`): `fresh` (the default) only its brief; `fork` (also
//! `fork=True`; native-host.ts `forkPoint`, `FORK_BRIEF_NOTE`) its parent's conversation so far, cut before the tool
//! call still running (the cell that spawns it), with a brief that says its kernel is new; `retrieve` (Rust-only) its
//! brief and where its parent's session is stored, which it searches and reads with `sessions` for what the brief left
//! out instead of carrying the whole conversation.

use serde_json::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Leads a forked child's brief: the conversation above is its parent's, and its kernel does not have its state.
/// How a subagent's context starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnContext {
    Fresh,
    Fork,
    Retrieve,
}

impl SpawnContext {
    /// `context=` and `fork=` of a spawn's options: `fork=True` is `context="fork"`, and the two must agree.
    pub fn of(context: Option<&Value>, fork: Option<&Value>) -> Result<SpawnContext, String> {
        let fork = match fork {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => return Err("fork must be True or False".into()),
        };
        let context = match context {
            None | Some(Value::Null) => None,
            Some(Value::String(text)) => Some(match text.as_str() {
                "fresh" => SpawnContext::Fresh,
                "fork" => SpawnContext::Fork,
                "retrieve" => SpawnContext::Retrieve,
                _ => return Err(CONTEXT_VALUES.into()),
            }),
            Some(_) => return Err(CONTEXT_VALUES.into()),
        };
        match (context, fork) {
            (None, true) | (Some(SpawnContext::Fork), _) => Ok(SpawnContext::Fork),
            (None, false) => Ok(SpawnContext::Fresh),
            (Some(context), false) => Ok(context),
            (Some(_), true) => Err("rlm.spawn fork=True is context=\"fork\": pass one of them".into()),
        }
    }
}

const CONTEXT_VALUES: &str = "context must be \"fresh\", \"fork\" or \"retrieve\"";

/// The brief of a `context="retrieve"` child starts with where its parent's session is and how to search it.
pub fn retrieve_brief_note(session_id: &str) -> String {
    format!(
        "You are a subagent started fresh: your parent's conversation is not in your context, but it is stored as \
         session {session_id}. When the brief below leaves out something you need (a decision, a path, an earlier \
         result), look it up there instead of guessing: `await sessions.search(\"regex\", sessions=[\"{session_id}\"])` \
         (or `terms=[...]`) finds its turns, each hit with a `span`; `await sessions.read(span, around=3)` reads one; \
         `await sessions.read(session=\"{session_id}\", limit=50)` reads it from the start. Do only the brief below."
    )
}

pub const FORK_BRIEF_NOTE: &str = "You are a subagent forked from the conversation above: it is your parent's context. Your REPL kernel is new: variables, imports and functions from earlier cells do not exist (files do). Do only the brief below.";

/// Where a forked child of `lane` starts: an opaque anchor naming the parent's conversation entry (the spawn reply's
/// `parent_branch_anchor`), none when the lane has no finished conversation yet, or an error.
pub type ForkPointHook = Arc<dyn Fn(&str) -> Result<Option<String>, String> + Send + Sync>;

/// `forkPoint`: over a branch's entries (oldest first, each with its id and the messages it holds), the newest entry
/// after which no tool call waits for its result; none for an empty branch.
pub fn fork_point<'a, I>(entries: I) -> Option<String>
where
    I: IntoIterator<Item = (String, &'a [Value])>,
{
    let mut pending: HashSet<String> = HashSet::new();
    let mut at = None;
    for (id, messages) in entries {
        for message in messages {
            match message["role"].as_str() {
                Some("assistant") => {
                    for part in message["content"].as_array().into_iter().flatten() {
                        if part["type"] == "toolCall"
                            && let Some(call) = part["id"].as_str()
                        {
                            pending.insert(call.to_string());
                        }
                    }
                }
                Some("toolResult") => {
                    if let Some(call) = message["toolCallId"].as_str() {
                        pending.remove(call);
                    }
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            at = Some(id);
        }
    }
    at
}
