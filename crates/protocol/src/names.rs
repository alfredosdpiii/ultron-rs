//! The names both ends of a connection spell the same: the services the server and its session workers expose, and
//! the error codes clients act on.

/// Chord's own service-control endpoint (packages/chord/src/services/wire.ts `SERVICE_CONTROL_ID`): `catalogue`,
/// `subscribe`, `unsubscribe`.
pub const CHORD_SERVICE: &str = "$chord.service";

/// Server-scoped: list, create, attach, remove sessions.
pub const SESSION_MANAGEMENT: &str = "ultron.session-management";
/// Server-scoped: the session store as a replicated listing.
pub const SESSION_DIRECTORY: &str = "ultron.session-directory";
pub const PRESENTATION_PLUGINS: &str = "ultron.presentation-plugins";

/// Session-scoped (a session worker's).
pub const AGENT_CONTROLLER: &str = "ultron.agent-controller";
pub const TRANSCRIPT: &str = "ultron.transcript";
pub const SESSION_CONTROL: &str = "ultron.session-control";
pub const EXTENSION_UI: &str = "ultron.extension-ui";
pub const MODELS: &str = "ultron.models";
pub const SCHEDULES: &str = "ultron.schedules";
pub const SESSION_PLUGINS: &str = "ultron.session-plugins";
pub const LEGACY_EXTENSION_COMMANDS: &str = "ultron.legacy-extension-commands";
/// A Rust addition: the session actor's operations (`{"op": ...}`).
pub const SESSION_OPS: &str = "ultron.session-ops";

/// The session's worker went away during the call; the server brings it back.
pub const WORKER_LOST_CODE: &str = "session_worker_lost";
/// The session's worker is being restarted; the call may be retried once it is back.
pub const RECONNECTING_CODE: &str = "session_reconnecting";
/// The worker is retiring (a newer release took over); the server routes the call to its successor.
pub const WORKER_RETIRING_CODE: &str = "session_worker_retiring";
pub const SESSION_NOT_FOUND_CODE: &str = "session_not_found";

/// The code of a call that failed because the session's worker went away: the session is durable, rejoin it.
pub fn is_lost_code(code: &str) -> bool {
    code == WORKER_LOST_CODE || code == RECONNECTING_CODE
}
