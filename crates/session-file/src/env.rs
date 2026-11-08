//! Clock, randomness, and agent directory: the process inputs the session manager reads.

/// Inputs the TypeScript implementation takes from the process (`Date`, `crypto`, the agent dir).
pub trait Env {
    /// Milliseconds since the Unix epoch (`Date.now()`).
    fn now_ms(&mut self) -> i64;
    /// A random UUID string (`crypto.randomUUID()`); entry ids use its first 8 characters.
    fn random_uuid(&mut self) -> String;
    /// A new session id (`uuidv7()`, whose generator state is process-wide upstream).
    fn session_id(&mut self) -> String;
    /// The agent directory (`getAgentDir()`): default session dirs live under `<agent>/sessions`.
    fn agent_dir(&self) -> String;
}

/// The real process environment: system clock, OS randomness, `ULTRON_CODING_AGENT_DIR` or `~/.ultron/agent`.
#[derive(Debug, Default)]
pub struct SystemEnv;

impl Env for SystemEnv {
    fn now_ms(&mut self) -> i64 {
        jsrt::date::now_ms()
    }

    fn random_uuid(&mut self) -> String {
        jsrt::random::uuid_v4()
    }

    fn session_id(&mut self) -> String {
        jsrt::random::uuid_v7(self.now_ms())
    }

    fn agent_dir(&self) -> String {
        let home = crate::paths::home_dir();
        config::paths::agent_dir(|name| std::env::var(name).ok(), std::path::Path::new(&home))
            .display()
            .to_string()
    }
}
