//! A guard's question about a borderline write (`Verdict::Ask`): the write is made, the question is settled off the
//! write path by the hooks' asker (the judge seat) and its answer comes with a later cell result; with no asker the
//! note tells the model to ask the user. Loki asks about credentials found only in test, fixture or example files.

use host::Guard;
use host::file_hooks::{Asker, FileHooks, FileWriteGuard, Verdict};
use host::loki_guard::{LokiGuard, LokiMode, borderline};
use serde_json::json;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Asks about every write.
struct Asking;

impl FileWriteGuard for Asking {
    fn name(&self) -> &str {
        "Loki"
    }
    fn before_write(&self, _: &Path, _: &str, _: &Path, _: &str) -> Result<Verdict, String> {
        Ok(Verdict::Ask("Is the key in it real?".into()))
    }
}

fn hooks(cwd: &Path, asker: Option<Asker>) -> FileHooks {
    let mut hooks = FileHooks::new(cwd);
    hooks.add(Box::new(Asking));
    if let Some(asker) = asker {
        hooks.on_ask(asker);
    }
    hooks
}

#[test]
fn host_guard_ask_writes_and_delivers_the_judges_answer_with_a_later_cell_result() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let asked: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = asked.clone();
    let hooks = hooks(
        &cwd,
        Some(Arc::new(move |question: &str| {
            seen.lock().unwrap().push(question.to_string());
            Ok(true)
        })),
    );
    let target = cwd.join("tests/keys.py").to_string_lossy().into_owned();
    assert_eq!(
        hooks.before_write("main", &target, "KEY = 'x'\n"),
        json!({"blocked": false, "notes": [
            "[Loki] Is the key in it real? It was written; the judge is asked, and its answer comes with a later cell result."
        ]})
    );
    hooks.settled();
    assert_eq!(*asked.lock().unwrap(), ["Is the key in it real?"]);
    assert_eq!(
        hooks.take_pending("main").as_deref(),
        Some(
            "[Loki] The judge answered the question about the write to tests/keys.py: it should not stand. Is the key in it real? Undo or fix it."
        )
    );
    assert_eq!(hooks.take_pending("main"), None);
    assert_eq!(hooks.stats()[0].blocked, 0);
}

#[test]
fn host_guard_ask_without_a_judge_answer_is_the_users() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let target = cwd.join("a.py").to_string_lossy().into_owned();
    // No asker at all: the note says so at once.
    assert_eq!(
        hooks(&cwd, None).before_write("main", &target, "x\n")["notes"],
        json!([
            "[Loki] Is the key in it real? It was written; ask the user whether it should stay (the write to a.py)."
        ])
    );
    // A judge that cannot settle it (abstained, unsure): the user decides.
    let unsure = hooks(
        &cwd,
        Some(Arc::new(|_: &str| Err("no judge backend is configured".into()))),
    );
    unsure.before_write("lane-1", &target, "x\n");
    unsure.settled();
    assert_eq!(
        unsure.take_pending("lane-1").as_deref(),
        Some(
            "[Loki] The judge could not settle the write to a.py (no judge backend is configured): Is the key in it real? Ask the user whether it should stay."
        )
    );
    // Cleared: one short line.
    let cleared = hooks(&cwd, Some(Arc::new(|_: &str| Ok(false))));
    cleared.before_write("main", &target, "x\n");
    cleared.settled();
    assert_eq!(
        cleared.take_pending("main").as_deref(),
        Some("[Loki] The judge cleared the write to a.py.")
    );
}

#[test]
fn host_loki_asks_about_a_credential_only_in_a_test_or_example_file() {
    let secret = "app.py:1: loki/secret: hardcoded credential (AWS access key)".to_string();
    let other = "app.py:2: loki/conflict-marker: unresolved merge conflict marker".to_string();
    assert!(borderline("tests/test_app.py", std::slice::from_ref(&secret)));
    assert!(borderline("pkg/fixtures/keys.json", std::slice::from_ref(&secret)));
    assert!(borderline("src/app.test.ts", std::slice::from_ref(&secret)));
    assert!(!borderline("src/app.py", std::slice::from_ref(&secret)));
    assert!(!borderline("tests/test_app.py", &[secret.clone(), other]));
    assert!(!borderline("tests/test_app.py", &[]));

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    let key = ["AKIA", "Q7XRJ2M4PL3ZWNVB"].concat();
    let content = format!("KEY = '{key}'\n");
    let guard = LokiGuard::new(root, LokiMode::On);
    let sample = guard
        .before_write(&root.join("tests/test_keys.py"), &content, root, "main")
        .unwrap();
    match sample {
        Verdict::Ask(question) => {
            assert!(question.contains("loki/secret"), "{question}");
            assert!(question.ends_with("or a test or sample value (no)?"), "{question}");
        }
        other => panic!("not asked: {other:?}"),
    }
    let real = guard
        .before_write(&root.join("src/keys.py"), &content, root, "main")
        .unwrap();
    assert!(matches!(real, Verdict::Block(_)), "{real:?}");
    // Advise mode reports instead of asking.
    let advise = LokiGuard::new(root, LokiMode::Advise)
        .before_write(&root.join("tests/test_keys.py"), &content, root, "main")
        .unwrap();
    assert!(matches!(advise, Verdict::Allow(Some(_))), "{advise:?}");
}
