//! The check library and its planted bugs (review::checks, review::eval::prove), the failure-mode register over
//! fates and review records (review::register), and Loki rules proposed from recurring review rules.

use review::checks::{Check, LIBRARY};
use review::eval::{Planted, clean_result, planted, prove};
use review::ledger::{Fate, FateKind};
use review::register::{build, from_disk, loki_proposals, normalize, repo_key};
use serde_json::{Value, json};

#[test]
fn review_checks_every_seeded_check_catches_its_planted_bug_and_is_quiet_without_it() {
    let names: Vec<&str> = LIBRARY.iter().map(|check| check.rule).collect();
    assert_eq!(names.len(), 10);
    assert_eq!(names[0], "Look before you transform");
    assert_eq!(names[7], "Make every check fail once");
    let proofs = prove(&LIBRARY, &planted());
    for proof in &proofs {
        assert!(proof.proven(), "{proof:?}");
    }
    // The clean result is quiet for every check.
    for check in &LIBRARY {
        assert_eq!((check.detect)(&clean_result()), Vec::<String>::new(), "{}", check.id);
    }
}

fn silent(_: &Value) -> Vec<String> {
    Vec::new()
}

fn loud(_: &Value) -> Vec<String> {
    vec!["always".into()]
}

#[test]
fn review_checks_a_check_counts_only_after_catching_its_planted_bug() {
    let library = [
        Check {
            id: "blind-spot",
            rule: "r",
            flags: "f",
            detect: silent,
        },
        Check {
            id: "noisy",
            rule: "r",
            flags: "f",
            detect: loud,
        },
        Check {
            id: "unplanted",
            rule: "r",
            flags: "f",
            detect: loud,
        },
    ];
    let cases = vec![
        Planted {
            check: "blind-spot",
            clean: json!({}),
            bugged: json!({"bug": true}),
        },
        Planted {
            check: "noisy",
            clean: json!({}),
            bugged: json!({"bug": true}),
        },
    ];
    let proofs = prove(&library, &cases);
    let summary: Vec<(&str, bool, bool, bool)> = proofs.iter().map(|p| (p.id, p.caught, p.quiet, p.proven())).collect();
    assert_eq!(
        summary,
        [
            ("blind-spot", false, true, false),
            ("noisy", true, false, false),
            ("unplanted", false, true, false)
        ]
    );
    // Unproven checks' hits are not counted in the register.
    let register = build(&[], &[json!({"result": clean_result()})], &library, &proofs);
    assert!(
        register.modes.iter().all(|mode| mode.source != "check"),
        "{:?}",
        register.modes
    );
    assert!(register.checks.iter().all(|check| !check.proven && check.hits == 0));
}

fn fate(id: &str, fate: FateKind, how: &str) -> Fate {
    Fate {
        id: id.into(),
        pull: "github.com/o/r#1".into(),
        file: "calc.py".into(),
        line: 4,
        category: "tests".into(),
        kind: "fast".into(),
        level: "medium".into(),
        claim: "c".into(),
        fate,
        how: how.into(),
        at: "2026-10-01T00:00:00Z".into(),
    }
}

#[test]
fn review_register_counts_rejected_fates_drops_unchecked_lines_and_proven_check_hits() {
    let fates = [
        fate("a", FateKind::Rejected, "thumbs down on the comment"),
        fate("b", FateKind::Rejected, "thumbs down on the comment"),
        fate("c", FateKind::Accepted, "fixed in a later commit"),
    ];
    let mut bugged = clean_result();
    bugged["findings"][0]["severity"] = json!("minor");
    bugged["dropped"]["contradicted"] = json!(2);
    bugged["notChecked"] = json!([
        "tests not run: no sandbox available",
        "src/a.py: generated file (12 lines)"
    ]);
    let mut other = clean_result();
    other["notChecked"] = json!([
        "tests not run: no sandbox available",
        "lib/b.py: generated file (3 lines)"
    ]);
    let records = [
        json!({"pull": "github.com/o/r#1", "result": bugged}),
        json!({"pull": "github.com/o/r#2", "result": other}),
    ];
    let register = build(&fates, &records, &LIBRARY, &prove(&LIBRARY, &planted()));
    assert_eq!((register.reviews, register.fates), (2, 3));
    let mode = |id: &str| {
        register
            .modes
            .iter()
            .find(|mode| mode.id == id)
            .unwrap_or_else(|| panic!("{id} in {:?}", register.modes))
    };
    let rejected = mode("fate:tests/fast:thumbs down on the comment");
    assert_eq!((rejected.count, rejected.source), (2, "fate"));
    assert_eq!(
        rejected.description,
        "tests findings from the fast pass rejected (thumbs down on the comment)"
    );
    assert_eq!(mode("dropped:contradicted").count, 2);
    assert_eq!(mode("dropped:rejected").count, 2);
    // One mode per line shape: paths and numbers taken out.
    assert_eq!(mode("notChecked:<path>: generated file (N lines)").count, 2);
    assert_eq!(mode("notChecked:tests not run: no sandbox available").count, 2);
    let conventions = mode("check:conventions");
    assert_eq!(conventions.count, 1);
    assert_eq!(
        conventions.examples,
        ["github.com/o/r#1: calc.py:4 is high but its severity says minor"]
    );
    let status = register.checks.iter().find(|check| check.id == "conventions").unwrap();
    assert!(status.proven && status.hits == 1);
    // Most frequent first.
    assert!(register.modes.windows(2).all(|pair| pair[0].count >= pair[1].count));
    assert_eq!(normalize("a/b/c.rs:12 failed 3 times"), "<path>:N failed N times");
}

fn rule(id: i64, body: &str, accepted: i64, rejected: i64) -> knowledge::RuleCredit {
    knowledge::RuleCredit {
        id,
        path: "src/app.py".into(),
        body: body.into(),
        url: format!("https://github.com/o/r/pull/1#discussion_r{id}"),
        accepted,
        rejected,
    }
}

#[test]
fn review_register_proposes_loki_rules_for_recurring_rules_only() {
    let proposals = loki_proposals(&[
        rule(7, "Don't call `print(` in library code. Use the logger.", 4, 1),
        rule(8, "Keep functions short", 5, 0),
        rule(9, "Never `eval(`", 2, 0),
        rule(10, "No `os.system(`", 3, 2),
    ]);
    let names: Vec<&str> = proposals.iter().map(|p| p.rule.as_str()).collect();
    assert_eq!(names, ["loki/review-r7", "loki/review-r8"]);
    assert_eq!(proposals[0].pattern.as_deref(), Some(r"print\("));
    assert_eq!(proposals[0].message, "Don't call `print(` in library code");
    assert_eq!(proposals[0].paths, "*.py");
    assert_eq!(
        proposals[0].tuple.as_deref(),
        Some(r##"("loki/review-r7", r#"print\("#, "Don't call `print(` in library code")"##)
    );
    // A rule that quotes no code needs a pattern from the person approving it.
    assert_eq!((proposals[1].pattern.clone(), proposals[1].tuple.clone()), (None, None));
}

#[test]
fn review_register_reads_the_state_records_and_knowledge_store_of_one_repository() {
    let dir = tempfile::tempdir().expect("dir");
    let root = dir.path().join("autoreview");
    std::fs::create_dir_all(root.join("reviews")).unwrap();
    let paths = review::settings::AutoreviewPaths {
        dir: root.display().to_string(),
        state: root.join("state.json").display().to_string(),
        logs: root.join("logs").display().to_string(),
        dry_run: root.join("dry-run").display().to_string(),
        cache: root.join("cache").display().to_string(),
        knowledge: dir.path().join("graph.db").display().to_string(),
    };
    let rejected =
        serde_json::to_value(fate("a", FateKind::Rejected, "the thread was resolved without a fix")).unwrap();
    std::fs::write(
        &paths.state,
        json!({"version": 1, "accounts": {}, "pulls": {}, "recent": [],
            "lessons": {"github.com/o/r": [rejected.clone()], "github.com/x/y": [rejected]}})
        .to_string(),
    )
    .unwrap();
    for (name, pull) in [("o-r-1.json", "github.com/o/r#1"), ("x-y-1.json", "github.com/x/y#1")] {
        std::fs::write(
            root.join("reviews").join(name),
            json!({"pull": pull, "result": clean_result()}).to_string(),
        )
        .unwrap();
    }
    let mut store = knowledge::Store::open(std::path::Path::new(&paths.knowledge)).unwrap();
    let note = knowledge::Note {
        pull: 1,
        path: "src/app.py".into(),
        line: Some(3),
        author: "alice".into(),
        body: "Don't call `print(` in library code.".into(),
        url: "u1".into(),
        at: "2026-10-01T00:00:00Z".into(),
        state: "addressed".into(),
        human: true,
        missed: false,
    };
    store.record_notes("github.com/o/r", &[note]).unwrap();
    let id = store.note_id("github.com/o/r", "u1").unwrap().unwrap();
    store
        .credit_rules("github.com/o/r", &[(id, true), (id, true), (id, true)])
        .unwrap();
    drop(store);

    let repo = repo_key("O/R");
    assert_eq!(repo, "github.com/o/r");
    let (register, proposals) = from_disk(&paths, Some(&repo));
    assert_eq!((register.reviews, register.fates), (1, 1));
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].rule, format!("loki/review-r{id}"));
    let (everything, _) = from_disk(&paths, None);
    assert_eq!((everything.reviews, everything.fates), (2, 2));
}
