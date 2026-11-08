//! Retry (`app.retry`, F5): the retryable-tail rule of omp turn-recovery.ts.

use interactive::retry::failed_turn_tail;
use serde_json::json;

fn assistant(stop: &str) -> serde_json::Value {
    json!({"role": "assistant", "content": [], "stopReason": stop})
}

fn result(error: bool) -> serde_json::Value {
    json!({"role": "toolResult", "toolName": "rlm", "content": [], "isError": error})
}

#[test]
fn interactive_retry_failed_tail_rule() {
    let user = json!({"role": "user", "content": "go"});
    assert!(failed_turn_tail(&[user.clone(), assistant("error")]));
    assert!(
        failed_turn_tail(&[user.clone(), assistant("aborted"), result(true)]),
        "placeholders trail the failed turn"
    );
    assert!(
        failed_turn_tail(&[user.clone(), assistant("toolUse"), result(true)]),
        "Esc landed during the tool call"
    );
    assert!(
        !failed_turn_tail(&[user.clone(), assistant("toolUse"), result(false)]),
        "a tool still in flight"
    );
    assert!(!failed_turn_tail(&[user.clone(), assistant("toolUse")]));
    assert!(
        !failed_turn_tail(&[user.clone(), assistant("stop")]),
        "a settled turn leaves nothing to retry"
    );
    assert!(!failed_turn_tail(&[user]));
    assert!(!failed_turn_tail(&[]));
}
