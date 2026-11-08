//! A [`ChildRunner`] that runs each subagent as a `crates/agent` loop: the caller supplies the agent options and the
//! model stream for a run (its tools, normally the child's own REPL bound to `run.host.lane_handler(&run.lane_name)`),
//! and the child's reply is its last assistant message's text. Cancelling the child aborts the loop.

use crate::host::{ChildRun, ChildRunner};
use agent::{Agent, AgentOptions, StreamFn};
use kernel::Signal;
use std::sync::Arc;

/// Builds a child's agent: options (model, system prompt, tools) and its model stream.
pub type ChildAgentFactory = Arc<dyn Fn(&ChildRun) -> (AgentOptions, Box<dyn StreamFn>) + Send + Sync>;

pub fn agent_runner(factory: ChildAgentFactory) -> ChildRunner {
    Arc::new(move |run: ChildRun| {
        let (options, stream) = factory(&run);
        let mut agent = Agent::new(options, stream);
        run_to_text(&mut agent, &run.prompt, &run.signal)
    })
}

/// Prompt `agent` with `prompt` on this thread (a current-thread runtime of its own) until the run ends; `signal`
/// aborting aborts the run. The reply is [`Agent::final_text`].
pub fn run_to_text(agent: &mut Agent, prompt: &str, signal: &Signal) -> Result<String, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let control = agent.control();
    let Some(listener) = signal.on_abort(move |_| control.abort()) else {
        return Err(signal.reason().unwrap_or_default());
    };
    runtime.block_on(agent.prompt_text(prompt, Vec::new()));
    signal.forget(listener);
    agent.final_text()
}
