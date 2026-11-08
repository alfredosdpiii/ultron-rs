//! `/review` expansion (packages/coding-agent/src/ultron/review.ts `expandReviewCommand`, applied by the session to
//! every prompt and queued message, as the TS AgentController does to `prompt`, `steer`, `followUp` and `nextRun`).
//! Replays crates/session-core/fixtures/review_command.json, recorded from the TypeScript controller by
//! generate-review-command.mjs, plus the ported literal expectations of
//! packages/coding-agent/test/ultron-review.test.ts ("/review command expansion").

use crate::p_session_support;
use crate::reopen_support;

use p_session_support::{in_order, text, user_texts};
use protocol::watch::QueueKind;
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::Op;
use session_core::review_command::{expand_review_command, review_prompt};

fn cases() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/review_command.json")).unwrap();
    fixture["cases"].as_array().unwrap().clone()
}

#[test]
fn session_core_review_command_expands_like_the_typescript_controller() {
    let cases = cases();
    assert!(cases.len() >= 80);
    for case in cases {
        let text = case["text"].as_str().unwrap();
        assert_eq!(expand_review_command(text), case["message"].as_str().unwrap(), "{case}");
    }
}

/// The session expands `/review` itself, for a prompt and for a queued message alike, whoever sent it (not only the
/// worker's clients); other text is left as typed.
#[test]
fn session_core_review_command_is_expanded_by_the_session() {
    let home = Home::new(None);
    let w = home.open(in_order(vec![text("one"), text("two")]), &[], None);
    w.call(Op::Enqueue {
        kind: QueueKind::NextRun,
        message: "/review main".into(),
        images: Vec::new(),
        entry_id: None,
    })
    .unwrap();
    let ended = w
        .call(Op::PromptAndWait {
            message: "/review".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ended["error"], Value::Null, "{ended}");
    let texts: Vec<Value> = user_texts(&w.requests()[0])
        .into_iter()
        .filter(|t| !t.as_str().is_some_and(|t| t.starts_with("[clock]")))
        .collect();
    assert_eq!(texts, [json!(review_prompt("main")), json!(review_prompt(""))]);
    w.call(Op::PromptAndWait {
        message: "please /review this".into(),
        images: Vec::new(),
    })
    .unwrap();
    let last = user_texts(w.requests().last().unwrap());
    assert_eq!(last.last(), Some(&json!("please /review this")));
}

/// ultron-review.test.ts "/review becomes a one-cell prompt ..." and "--post adds the explicit-confirmation rule".
#[test]
fn session_core_review_command_ported_typescript_expectations() {
    let prompt = expand_review_command("/review");
    assert!(prompt.contains("review = await review_api.run(rlm, \"\")"));
    assert!(prompt.contains("print(review.report)"));
    assert!(!prompt.contains("post"));
    let with_args = expand_review_command("/review main --only sec,bugs \"src/a b.py\"");
    assert_eq!(
        with_args.split('\n').next().unwrap(),
        "/review main --only sec,bugs \"src/a b.py\""
    );
    assert!(with_args.contains(r#"review_api.run(rlm, "main --only sec,bugs \"src/a b.py\"")"#));
    let post = review_prompt("12 --post");
    let rule = post.find("only after I answer yes").unwrap();
    assert!(post[rule..].contains("confirm=True"));
    assert_eq!(expand_review_command("/reviewer x"), "/reviewer x");
    assert_eq!(expand_review_command("please /review this"), "please /review this");
}
