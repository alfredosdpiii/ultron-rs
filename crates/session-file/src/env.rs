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
        match std::env::var("ULTRON_CODING_AGENT_DIR") {
            Ok(dir) if !dir.is_empty() => crate::paths::normalize_path(&dir),
            _ => crate::paths::join(&[&crate::paths::home_dir(), ".ultron", "agent"]),
        }
    }
}

/// An owned `uuidv7()` generator (fixtures and test clocks); the process-wide one is [`jsrt::random::uuid_v7`].
#[derive(Debug, Default)]
pub struct Uuidv7(jsrt::random::Uuidv7);

impl Uuidv7 {
    /// The next id at `now_ms` (clamped to never go backwards) with fresh random bytes.
    pub fn next(&mut self, now_ms: i64, bytes: [u8; 16]) -> String {
        self.0.generate(None, now_ms, bytes)
    }
}
