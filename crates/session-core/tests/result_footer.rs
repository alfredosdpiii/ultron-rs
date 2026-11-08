//! REPL result footers in the product's requests (the global `rlm.resultFooter` setting,
//! session_core::lane::add_result_footers): each tool result ends with `[t+... ctx ...]`, derived from the stored log,
//! so an earlier result's footer reads the same in every later request, and the runtime guide says so. Off by
//! default, guide line included. New in Rust: literal expectations.

use crate::p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool, tool_results};
use serde_json::{Value, json};

const GUIDE_LINE: &str =
    "Each rlm result ends with `[t+<time since the session started> ctx <tokens of the request that ran it>]`.";

fn system_of(request: &Value) -> String {
    request["system"].to_string()
}

fn footer_of(result: &Value) -> String {
    let text = match result {
        Value::String(text) => text.clone(),
        other => other
            .as_array()
            .and_then(|b| b.last())
            .map(|b| b["text"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default(),
    };
    text.rsplit('\n').next().unwrap_or_default().to_string()
}

#[test]
fn session_core_result_footers_end_each_result_and_stay_the_same_across_requests() {
    let script = in_order(vec![
        tool("toolu_1", "print('one')"),
        tool("toolu_2", "print('two')"),
        text("done"),
    ]);
    let a = actor(
        script,
        Setup {
            settings: Some(json!({"rlm": {"resultFooter": true}})),
            ..Setup::default()
        },
    );
    assert_eq!(a.prompt("go")["error"], Value::Null);
    let requests = a.requests();
    let second = tool_results(&requests[1]);
    let third = tool_results(&requests[2]);
    let first_footer = footer_of(&second[0]);
    let footer = regex::Regex::new(r"^\[t\+[0-9]+s ctx [0-9]+\]$").unwrap();
    assert!(footer.is_match(&first_footer), "{second:?}");
    assert!(second[0].to_string().starts_with("\"one"), "{second:?}");
    // The next request carries the first result exactly as before, then the second with its own footer.
    assert_eq!(third[0], second[0]);
    assert!(footer.is_match(&footer_of(&third[1])), "{third:?}");
    assert!(
        system_of(&requests[0]).contains(GUIDE_LINE),
        "the guide explains the footer"
    );
}

#[test]
fn session_core_result_footers_are_off_by_default() {
    let a = actor(
        in_order(vec![tool("toolu_1", "print('one')"), text("done")]),
        Setup::default(),
    );
    assert_eq!(a.prompt("go")["error"], Value::Null);
    let requests = a.requests();
    assert_eq!(tool_results(&requests[1]), [Value::String("one".into())]);
    assert!(!system_of(&requests[0]).contains("[t+"), "no footer line in the guide");
}
