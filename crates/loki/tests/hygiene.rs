//! Hygiene advisories (`loki::hygiene`): the vendored codehygiene model and nits report only the lines a change
//! added, only the default kinds unless asked for all, and each says "NOT CHECKED" rather than nothing when it cannot
//! run.

use loki::hygiene::{self, Level, Squiggle};
use loki::proc::Deadline;
use serde_json::json;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn squiggle(line: usize, kind: &str) -> Squiggle {
    Squiggle {
        line,
        kind: kind.into(),
        message: "why".into(),
    }
}

/// Whether a Python with NumPy is here; the tests that run the model pass without one, as the Ruff tests do.
fn model_runs() -> bool {
    hygiene::score(&["x = 1\n"], Deadline::after(60.0)).is_ok()
}

const OLD: &str = "def load(path):\n    try:\n        return open(path).read()\n    except Exception:\n        pass\n";
const NEW: &str =
    "\n\ndef save(path, text):\n    try:\n        open(path, 'w').write(text)\n    except Exception:\n        pass\n";

#[test]
fn hygiene_notes_cover_added_lines_of_the_default_kinds() {
    let (before, after) = ("a = 1\nb = 2\n", "a = 1\nnew = 3\nb = 2\nlonger = 4\n");
    let found = [
        squiggle(1, "swallowed_error"),
        squiggle(2, "swallowed_error"),
        squiggle(4, "dense_line"),
    ];
    assert_eq!(
        hygiene::notes("src/a.py", before, after, &found, Level::On),
        ["src/a.py:2: loki/hygiene-swallowed-error: why: `new = 3`"],
        "line 1 was there before the change, and dense_line is not a default kind"
    );
    assert_eq!(
        hygiene::notes("src/a.py", before, after, &found, Level::All),
        [
            "src/a.py:2: loki/hygiene-swallowed-error: why: `new = 3`",
            "src/a.py:4: loki/hygiene-dense-line: why: `longer = 4`"
        ]
    );
    assert!(
        hygiene::notes("src/a.py", after, after, &found, Level::All).is_empty(),
        "nothing was added"
    );
}

#[test]
fn hygiene_level_follows_the_variable_then_the_setting_and_is_on_by_default() {
    let saved = |value: serde_json::Value| loki::settings::saved(json!({"loki": value}).as_object().unwrap());
    assert_eq!(hygiene::level(None, &saved(json!({}))), Level::On);
    assert_eq!(hygiene::level(None, &saved(json!({"hygiene": "off"}))), Level::Off);
    assert_eq!(hygiene::level(None, &saved(json!({"hygiene": "all"}))), Level::All);
    assert_eq!(
        hygiene::level(None, &saved(json!({"hygiene": "loud"}))),
        Level::On,
        "an invalid value is dropped"
    );
    assert_eq!(
        hygiene::level(Some("off"), &saved(json!({"hygiene": "all"}))),
        Level::Off
    );
    assert_eq!(
        hygiene::level(Some(" ALL "), &saved(json!({"hygiene": "off"}))),
        Level::All
    );
    assert_eq!(
        hygiene::level(Some(""), &saved(json!({"hygiene": "off"}))),
        Level::Off,
        "an empty variable is unset"
    );
}

#[test]
fn hygiene_reports_the_swallowed_error_a_write_added_not_the_one_already_committed() {
    if !model_runs() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("store.py"), OLD).unwrap();
    std::fs::write(root.join("notes.md"), "x\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let files = ["store.py".to_string(), "notes.md".to_string()];
    assert!(
        hygiene::written(&root, &files, Level::On).is_empty(),
        "an unchanged file's old debt is not reported"
    );
    std::fs::write(root.join("store.py"), format!("{OLD}{NEW}")).unwrap();
    let notes = hygiene::written_with(&root, &files, Level::On, &[]);
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(
        notes[1].starts_with("NOT CHECKED nits: ") && notes[1].contains("uv run --no-project --with git+https://"),
        "without nits the model's note still arrives, and the gap names the command that fetches nits: {notes:?}"
    );
    assert!(
        notes[0].starts_with("store.py:11: loki/hygiene-swallowed-error: ")
            && notes[0].ends_with("`except Exception:`"),
        "{notes:?}"
    );
    assert!(hygiene::written(&root, &files, Level::Off).is_empty());
    // Once committed the line is no longer new to HEAD; against the base it still is.
    git(&root, &["add", "."]);
    git(
        &root,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "-m",
            "more",
        ],
    );
    assert!(hygiene::written(&root, &files, Level::On).is_empty());
    let from_model = |notes: Vec<String>| -> Vec<String> {
        notes
            .into_iter()
            .filter(|note| note.contains(": loki/hygiene-"))
            .collect()
    };
    assert_eq!(
        from_model(hygiene::changed(&root, "HEAD~1", Level::On)),
        from_model(notes)
    );
    assert!(hygiene::changed(&root, "HEAD", Level::On).is_empty());
    assert!(hygiene::changed(&root, "HEAD~1", Level::Off).is_empty());
}

#[test]
fn hygiene_skips_files_it_does_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let files = [
        "notes.md".to_string(),
        "vendor/lib/a.py".to_string(),
        "gone.py".to_string(),
    ];
    std::fs::create_dir_all(dir.path().join("vendor/lib")).unwrap();
    std::fs::write(dir.path().join("notes.md"), "x\n").unwrap();
    std::fs::write(dir.path().join("vendor/lib/a.py"), OLD).unwrap();
    assert!(
        hygiene::written(dir.path(), &files, Level::All).is_empty(),
        "prose, vendored code and a missing file start no model run and no note"
    );
}

#[test]
fn hygiene_reports_one_note_where_nits_and_the_model_agree_and_it_is_the_nits_one() {
    let (before, after) = ("a = 1\n", "a = 1\nexcept Exception:\nurl = 2\n");
    let model = [
        squiggle(1, "noisy_comment"),
        squiggle(2, "swallowed_error"),
        squiggle(3, "noisy_comment"),
    ];
    let nits = [
        squiggle(1, "config_fallback"),
        squiggle(2, "swallowed_error"),
        squiggle(3, "config_fallback"),
    ];
    assert_eq!(
        hygiene::merged("a.py", before, after, &model, &nits, Level::On),
        [
            "a.py:2: loki/nits-swallowed-error: why: `except Exception:`",
            "a.py:3: loki/nits-config-fallback: why: `url = 2`",
            "a.py:3: loki/hygiene-noisy-comment: why: `url = 2`",
        ],
        "line 1 was committed; on line 3 the two report different kinds, so both stay"
    );
    assert!(hygiene::merged("a.py", before, after, &model, &nits, Level::Off).is_empty());
}

#[test]
fn nits_is_only_ever_run_from_what_is_already_on_the_machine() {
    for launcher in loki::nits::launchers().iter().skip(1) {
        assert!(launcher.contains(&"--offline".to_string()), "{launcher:?}");
        assert!(
            launcher.iter().any(|arg| arg.ends_with(loki::nits::COMMIT)),
            "{launcher:?}"
        );
    }
}

const FALLBACK: &str = "import os\n\nURL = os.getenv('DATABASE_URL', 'localhost')\n";

#[test]
fn nits_reports_what_a_write_added_and_its_low_precision_rules_only_when_asked_for_all() {
    let launchers = loki::nits::launchers();
    let probe = [("a.py", "x = 1\n".to_string())];
    if loki::nits::score(&launchers, &probe, false, Deadline::after(60.0)).is_err() {
        return; // nits is neither installed nor in uv's cache here
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("old.py"), FALLBACK).unwrap();
    std::fs::write(root.join("store.py"), "LIMIT = 10\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    std::fs::write(root.join("old.py"), format!("{FALLBACK}LIMIT = 10\n")).unwrap();
    std::fs::write(root.join("store.py"), format!("{FALLBACK}{NEW}")).unwrap();
    let files = ["old.py".to_string(), "store.py".to_string()];
    let nits_only = |level| -> Vec<String> {
        hygiene::written_with(&root, &files, level, &launchers)
            .into_iter()
            .filter(|note| note.contains(": loki/nits-"))
            .collect()
    };
    let on = nits_only(Level::On);
    assert_eq!(on.len(), 1, "old.py's committed fallback is not reported: {on:?}");
    assert!(
        on[0].starts_with("store.py:3: loki/nits-config-fallback: p0.95 ") && on[0].ends_with("'localhost')`"),
        "{on:?}"
    );
    let all = nits_only(Level::All);
    assert!(
        all.iter()
            .any(|note| note.starts_with("store.py:9: loki/nits-swallowed-error: p0.")),
        "a Python swallowed error measures under 0.80, so nits reports it only at `all`: {all:?}"
    );
    if model_runs() {
        let both = hygiene::written_with(&root, &files, Level::All, &launchers);
        let on_line = both
            .iter()
            .filter(|note| note.starts_with("store.py:9: ") && note.contains("swallowed-error"));
        assert_eq!(on_line.count(), 1, "{both:?}");
    }
}
