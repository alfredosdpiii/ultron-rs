//! Working-tree checkpoints of a root's turns (Rust-only feature; see docs/rust-rewrite/features.md): each root turn
//! records the working tree as an unreferenced commit (crates/delegation `checkpoints`) under its key
//! (crates/session-journal `checkpoints`), before the turn runs: the native root under its prompt entry
//! (crate::core), an external root (`ultron claude`) under its turn id.
//!
//! Settings: `checkpoints.enabled` (default true; only Git or jj working trees record anything),
//! `checkpoints.maxFiles`, `checkpoints.maxFileBytes`. Off while the shell sandbox is on (`sandbox.mode` other than
//! `off`): the checkpoint's Git runs on the host, outside the sandbox, and would obey hooks, filters and the
//! fsmonitor that sandboxed code can write into the repository's config (Mirror disables checkpoints in a sandbox
//! for the same reason).

use crate::durable_store::{SharedSession, lock};
use crate::lane::Lane;
use delegation::checkpoints::{self as vcs, Capture, Checkpoint, Limits};
use serde_json::{Value, json};
use session_journal::checkpoints as stored;
use std::path::PathBuf;

/// What a turn's checkpoint needs: where, under which settings, with which `jj` (from the session's PATH).
pub struct TurnCheckpoints {
    pub session: SharedSession,
    pub cwd: PathBuf,
    /// The merged settings' `checkpoints` object.
    pub settings: Value,
    pub sandboxed: bool,
    pub jj: Option<PathBuf>,
}

impl TurnCheckpoints {
    /// The checkpoints of `lane`'s turns in `cwd`, under the merged `settings`, with `jj` from the session's `path`.
    pub fn of_lane(session: SharedSession, cwd: PathBuf, settings: &Value, lane: &Lane, path: Option<&str>) -> Self {
        let sandboxed = lane
            .stack
            .host
            .launcher()
            .is_some_and(|launcher| launcher.mode != sandbox::settings::Mode::Off);
        TurnCheckpoints {
            session,
            cwd,
            settings: settings["checkpoints"].clone(),
            sandboxed,
            jj: on_path(path, "jj"),
        }
    }

    pub fn enabled(&self) -> bool {
        self.settings["enabled"] != false && !self.sandboxed
    }

    pub fn limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_files: self.settings["maxFiles"]
                .as_u64()
                .map_or(defaults.max_files, |n| n as usize),
            max_file_bytes: self.settings["maxFileBytes"]
                .as_u64()
                .unwrap_or(defaults.max_file_bytes),
        }
    }

    /// Record the working tree for the turn `key` before it runs, the record naming the turn as `field` (`entryId`
    /// or `turn`). Never fails the turn: a tree that could not be recorded keeps a note instead.
    pub fn record(&self, field: &str, key: &str) {
        if !self.enabled() || key.is_empty() {
            return;
        }
        let previous = stored::latest_taken(&lock(&self.session)).and_then(|v| Checkpoint::from_json(&v));
        let captured = vcs::capture(&self.cwd, previous.as_ref(), &self.limits(), self.jj.as_deref());
        let mut value = match captured {
            Ok(Capture::Outside) => return,
            Ok(Capture::Taken(checkpoint)) => checkpoint.to_json(),
            Ok(Capture::Unchanged(checkpoint)) => {
                let mut value = checkpoint.to_json();
                value["unchanged"] = json!(true);
                value
            }
            Ok(Capture::Skipped(note)) | Err(note) => json!({"skipped": note}),
        };
        value["kind"] = json!("turn");
        value[field] = json!(key);
        value["createdAt"] = json!(jsrt::date::now_ms());
        let _ = stored::put(&mut lock(&self.session), key, value);
    }
}

/// The first executable `name` on `path` (the session's PATH, never the process's).
pub fn on_path(path: Option<&str>, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}
