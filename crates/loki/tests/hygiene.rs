//! Hygiene advisories (`loki::hygiene`): the vendored codehygiene model reports only the lines a change added, only
//! the default kinds unless asked for all, and says "NOT CHECKED" rather than nothing when it cannot run.

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
    let notes = hygiene::written(&root, &files, Level::On);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("store.py:11: loki/hygiene-swallowed-error: ")
            && notes[0].ends_with("`except Exception:`"),
        "{notes:?}"
    );
    assert!(hygiene::written(&root, &files, Level::Off).is_empty());
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
