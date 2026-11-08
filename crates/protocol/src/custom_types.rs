//! The custom message types (`role: custom`, `customType`) the engine adds to a conversation: clients draw each as one
//! line, the model sees its text.

/// An auto-continue nudge (the persona's, or keep-going mode's).
pub const NUDGE: &str = "ultron.nudge";
/// Long-term memories recalled for a user message.
pub const RECALLED_MEMORY: &str = "ultron.memory";
/// The active instruction refinements.
pub const REFINEMENTS: &str = "ultron.refinements";
/// A fired schedule's prompt.
pub const SCHEDULE: &str = "ultron.schedule";
