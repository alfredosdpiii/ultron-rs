//! Worktree children across a restart (B-report #3e, #8): a kept worktree already merged stays merged for the next
//! worker (`rlm.merge` says so instead of merging it again), and a reattached child's worktree record names the
//! worker that now runs it, so that worker's close prunes it.

use delegation::host::{ChildRunner, Delegation, DelegationOptions, Reattached};
use delegation::worktrees as wt;
use kernel::Signal;
use kernel::store::DocumentStore;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HANG: Duration = Duration::from_secs(120);

/// One task journal two hosts open in turn (two workers of one session).
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Option<Value>>>);

impl DocumentStore for Shared {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn git(cwd: &Path, args: &[&str]) {
    let quiet = ["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"];
    let out = Command::new("git")
        .args(quiet)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
}

fn repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().canonicalize().unwrap().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.com"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "initial"]);
    (temp, repo)
}

fn host(store: &Shared, repo: &Path, runner: ChildRunner) -> Delegation {
    let mut options = DelegationOptions::new(runner);
    options.store = Box::new(store.clone());
    options.workspace = Some(repo.to_path_buf());
    options.keep_worktrees = true;
    Delegation::new(options).0
}

/// A child that writes `new.txt` in its worktree and replies.
fn writer() -> ChildRunner {
    Arc::new(|run| {
        std::fs::write(PathBuf::from(run.cwd.unwrap()).join("new.txt"), "new\n").unwrap();
        Ok("done".into())
    })
}

#[test]
fn delegation_a_kept_worktree_merged_before_a_restart_stays_merged() {
    let (_temp, repo) = repo();
    let store = Shared::default();
    let first = host(&store, &repo, writer());
    let spawned = first
        .handle(
            "rlm.spawn",
            json!({"prompt": "add", "kwargs": {"name": "editor", "worktree": true}}),
            "main",
        )
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    first.wait(std::slice::from_ref(&id), Some(HANG));
    let merged = first.handle("rlm.merge", json!({}), "main").unwrap();
    assert_eq!(merged["results"][0]["status"], "merged", "{merged}");
    drop(first);

    let second = host(&store, &repo, writer());
    second.restore(&[], |_| None).unwrap();
    let again = second.handle("rlm.merge", json!({"selectors": [id]}), "main").unwrap();
    assert_eq!(
        (
            again["results"][0]["status"].clone(),
            again["results"][0]["note"].clone()
        ),
        (json!("merged"), json!("already merged")),
        "{again}"
    );
}

#[test]
fn delegation_a_reattached_child_worktree_is_owned_by_the_worker_that_runs_it() {
    let (_temp, repo) = repo();
    let store = Shared::default();
    let first = host(
        &store,
        &repo,
        Arc::new(|run| {
            run.signal.wait(HANG);
            Err("the first worker went away".into())
        }),
    );
    let spawned = first
        .handle(
            "rlm.spawn",
            json!({"prompt": "add", "kwargs": {"name": "editor", "worktree": true}}),
            "main",
        )
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    // The first worker died: its record names a process that is gone.
    let mut record = wt::list_records(&repo)
        .into_iter()
        .find(|record| record.task_id.as_deref() == Some(id.as_str()))
        .unwrap();
    record.owner.pid = u32::MAX;
    wt::save_record(&record).unwrap();

    let second = host(&store, &repo, writer());
    second
        .restore(std::slice::from_ref(&id), |_| {
            Some(Reattached {
                waiter: Box::new(|_: Signal| Ok("done".into())),
                contract: None,
                run: None,
            })
        })
        .unwrap();
    let owner = wt::list_records(&repo)
        .into_iter()
        .find(|record| record.task_id.as_deref() == Some(id.as_str()))
        .unwrap()
        .owner;
    assert_eq!(owner, wt::Owner::of_this_process());
    second.wait(std::slice::from_ref(&id), Some(HANG));
    second.close();
    first.close();
}
