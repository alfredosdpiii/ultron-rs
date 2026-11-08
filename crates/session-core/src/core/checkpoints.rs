//! The root lane's working-tree checkpoints (crate::turn_checkpoints) and the file-restoring rewind:
//! [`Op::RestoreFiles`](super::Op::RestoreFiles) writes a checkpoint's files back after saving the current ones as
//! another checkpoint.

use super::Core;
use crate::durable_store::lock as lock_session;
use crate::turn_checkpoints::TurnCheckpoints;
use delegation::checkpoints::{self as vcs, Checkpoint};
use jsrt::date::now_ms;
use serde_json::{Value, json};
use session_journal::checkpoints::{self as stored, RESTORE_PREFIX};

/// A restore while a run streams.
pub const RESTORE_WHILE_STREAMING: &str = "Wait for the current response to finish before restoring files.";
/// A restore inside the shell sandbox.
pub const RESTORE_IN_SANDBOX: &str =
    "Checkpoints are off while the shell sandbox is on (sandbox.mode): their Git would run outside the sandbox.";

impl Core {
    fn checkpoints(&self) -> TurnCheckpoints {
        TurnCheckpoints::of_lane(
            self.session.clone(),
            self.env.cwd.clone(),
            &self.runtime.config.settings,
            &self.lane,
            (self.env.var)("PATH").as_deref(),
        )
    }

    /// Record the working tree for the root turn whose prompt is `prompt_entry` (before the turn runs).
    pub(super) fn checkpoint_turn(&mut self, prompt_entry: &str) {
        self.checkpoints().record("entryId", prompt_entry);
    }

    /// [`Op::Checkpoints`](super::Op::Checkpoints): `{checkpoints: [{key, kind, entryId, commit, createdAt,
    /// skipped?, unchanged?}]}`, oldest first, and whether new ones are recorded.
    pub(super) fn list_checkpoints(&self) -> Value {
        let listed: Vec<Value> = stored::list(&lock_session(&self.session))
            .into_iter()
            .map(|(key, value)| {
                let mut item = json!({
                    "key": key, "kind": value["kind"], "entryId": value["entryId"], "commit": value["commit"],
                    "createdAt": value["createdAt"],
                });
                for extra in ["skipped", "unchanged", "restoredTo"] {
                    if !value[extra].is_null() {
                        item[extra] = value[extra].clone();
                    }
                }
                item
            })
            .collect();
        json!({"checkpoints": listed, "enabled": self.checkpoints().enabled()})
    }

    /// [`Op::RestoreFiles`](super::Op::RestoreFiles): write the files of checkpoint `key`, else of the checkpoint
    /// that describes entry `entry_id` (its turn's start), back into the working tree. The current files are saved
    /// first as a `restore:` checkpoint on the branch tip, so the restore can be undone the same way.
    pub(super) fn restore_files(&mut self, entry_id: Option<String>, key: Option<String>) -> Result<Value, String> {
        if self.run.is_some() {
            return Err(RESTORE_WHILE_STREAMING.into());
        }
        let checkpoints = self.checkpoints();
        if checkpoints.sandboxed {
            return Err(RESTORE_IN_SANDBOX.into());
        }
        let found = {
            let session = lock_session(&self.session);
            match (&key, &entry_id) {
                (Some(key), _) => session
                    .value(session_journal::ns::CHECKPOINTS, key)
                    .cloned()
                    .map(|value| (key.clone(), value)),
                (None, Some(entry)) => stored::for_entry(&session, entry),
                (None, None) => return Err("Name a checkpoint or an entry to restore files to".into()),
            }
        };
        let Some((key, value)) = found else {
            return Err(
                "No checkpoint of the files at this point (checkpoints start with the first turn in a Git repository)"
                    .into(),
            );
        };
        if let Some(note) = value["skipped"].as_str() {
            return Err(format!("The files at this point were not recorded: {note}"));
        }
        let target = Checkpoint::from_json(&value).ok_or("The checkpoint record is incomplete")?;
        let restored = vcs::restore(&self.env.cwd, &target, &checkpoints.limits(), checkpoints.jj.as_deref())?;
        let short = |commit: &str| commit[..commit.len().min(12)].to_string();
        let already = target.vcs == "git" && restored.safety.tree == target.tree;
        if already {
            return Ok(json!({
                "checkpoint": key, "commit": target.commit, "written": 0, "deleted": 0, "safety": null,
                "message": format!("Files already match checkpoint {}", short(&target.commit)),
            }));
        }
        let tip = self.tip();
        let safety_key = format!(
            "{RESTORE_PREFIX}{}",
            session_journal::lock_clock(&lock_session(&self.session).clock()).next_id()
        );
        let mut safety = restored.safety.to_json();
        safety["kind"] = json!("restore");
        safety["entryId"] = tip.map_or(Value::Null, Value::from);
        safety["restoredTo"] = json!(key);
        safety["createdAt"] = json!(now_ms());
        stored::put(&mut lock_session(&self.session), &safety_key, safety)?;
        let counts = if target.vcs == "git" {
            format!(
                " ({} written, {} deleted)",
                restored.written.len(),
                restored.deleted.len()
            )
        } else {
            String::new()
        };
        Ok(json!({
            "checkpoint": key, "commit": target.commit, "written": restored.written.len(),
            "deleted": restored.deleted.len(), "safety": {"key": safety_key, "commit": restored.safety.commit},
            "message": format!(
                "Restored files to checkpoint {}{counts}. The files from before are checkpoint {}: restore the current entry in /tree to undo.",
                short(&target.commit),
                short(&restored.safety.commit)
            ),
        }))
    }
}
