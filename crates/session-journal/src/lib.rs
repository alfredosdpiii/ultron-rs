//! Ultron's format-4 session journal: the JSONL file the TypeScript product writes for every session
//! (`packages/agent/src/harness/session/{commit,in-memory-storage-state,session,context,fork-policy}.ts` and
//! `jsonl/*`). A file is a header line (`{"v":4,"kind":"header",...}`) followed by one committed transaction per
//! line; replaying the transactions gives the session's entries (a tree through `parentId`), scalar values (branch
//! tips, lane configuration and state, operation state, names, labels, Ultron documents), lists and usage rows.
//!
//! Legacy Pi v3 files (`crates/session-file`) are read as format-4 writes and upgraded in place by their first
//! commit. [`run`] writes the harness operation journal of a prompted run; [`names`] lists sessions with their
//! names and resolves `--resume <name|id>`. [`search`] lists, greps and reads stored sessions read-only, citing
//! what it finds as spans (`session://<id>#<from>..<to>`).

pub mod catalog;
pub mod checkpoints;
pub mod codec;
pub mod context;
pub mod fork;
pub mod io;
pub mod legacy_v3;
pub mod names;
pub mod ns;
pub mod repo;
pub mod run;
pub mod search;
pub mod session;
pub mod state;
pub mod storage;
pub mod usage;

pub use fork::ForkOptions;
pub use repo::{Metadata, SessionRepo};
pub use session::Session;
pub use state::Write;
pub use storage::JsonlStorage;

use std::sync::{Arc, Mutex, MutexGuard};

/// The clock and id source of a session (`Date.now()` and `uuidv7()` of `@ultron/ai`).
pub trait Clock: Send {
    fn now(&mut self) -> i64;
    /// `uuidv7(at)`: an id whose timestamp is exactly `at` (a follower id; the ordinary clock is unaffected).
    fn uuidv7(&mut self, at: i64) -> String;
    /// `uuidv7()`: an id at the current time, never earlier than the previous ordinary id.
    fn next_id(&mut self) -> String;
}

pub type SharedClock = Arc<Mutex<dyn Clock>>;

/// Lock a shared clock (a poisoned lock is still usable: the clock has no invariants to break).
pub fn lock_clock(clock: &SharedClock) -> MutexGuard<'_, dyn Clock + 'static> {
    jsrt::sync::lock(clock)
}

/// The system clock with the process-wide UUIDv7 generator and OS randomness.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&mut self) -> i64 {
        jsrt::date::now_ms()
    }

    fn uuidv7(&mut self, at: i64) -> String {
        jsrt::random::uuid_v7_exact(at)
    }

    fn next_id(&mut self) -> String {
        jsrt::random::uuid_v7(jsrt::date::now_ms())
    }
}

impl SystemClock {
    pub fn shared() -> SharedClock {
        Arc::new(Mutex::new(SystemClock))
    }
}

/// A deterministic clock for tests and fixtures: `now()` reads `time` and advances it by `step`; ids use zero
/// random bytes (the fixture generators pin `crypto.getRandomValues` to zeros the same way).
#[derive(Debug)]
pub struct TestClock {
    pub time: i64,
    pub step: i64,
    generator: jsrt::random::Uuidv7,
}

impl TestClock {
    pub fn new(start: i64, step: i64) -> Self {
        TestClock {
            time: start,
            step,
            generator: jsrt::random::Uuidv7::new(),
        }
    }

    pub fn shared(start: i64, step: i64) -> SharedClock {
        Arc::new(Mutex::new(TestClock::new(start, step)))
    }
}

impl Clock for TestClock {
    fn now(&mut self) -> i64 {
        let now = self.time;
        self.time += self.step;
        now
    }

    fn uuidv7(&mut self, at: i64) -> String {
        self.generator.generate(Some(at), 0, [0; 16])
    }

    fn next_id(&mut self) -> String {
        let now = self.now();
        self.generator.generate(None, now, [0; 16])
    }
}
