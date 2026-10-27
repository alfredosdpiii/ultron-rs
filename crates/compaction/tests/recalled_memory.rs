//! compaction::convert_for_summary (Rust-only): a summary's conversation leaves out recalled long-term memory, so
//! what was recalled is never summarized, and kept, again; every other message converts as `convert_to_llm` does.

use compaction::{RECALLED_MEMORY_TYPE, convert_for_summary, convert_to_llm, serialize_conversation};
use serde_json::json;

#[test]
fn compaction_summary_input_leaves_out_recalled_memory() {
    let recalled = json!({"role": "custom", "customType": RECALLED_MEMORY_TYPE, "display": true,
        "content": "[memory] Long-term memory recalled.\n- (project) Deploys go through staging.", "timestamp": 1});
    let schedule = json!({"role": "custom", "customType": "ultron.schedule", "content": "nightly run", "timestamp": 2});
    let user = json!({"role": "user", "content": [{"type": "text", "text": "ship it"}], "timestamp": 3});
    let messages = [recalled, schedule.clone(), user.clone()];
    let summary = serialize_conversation(&convert_for_summary(&messages));
    assert!(!summary.contains("Deploys go through staging"), "{summary}");
    assert_eq!(
        summary,
        serialize_conversation(&convert_to_llm(&[schedule, user])),
        "other custom messages stay"
    );
}
