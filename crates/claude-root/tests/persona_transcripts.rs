//! The Stop hook's reading of a Claude Code transcript (`persona_hook::last_turn`) replayed against the transcripts
//! of crates/resources/fixtures/persona-nudge.json (recorded from the user's Claude Code hook by its
//! generate-persona-nudge.py), and the nudge rule on what it read.

use claude_root::persona_hook::last_turn;
use resources::persona::should_nudge;
use serde_json::Value;

#[test]
fn claude_root_persona_last_turn_matches_the_recorded_hook() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../resources/fixtures/persona-nudge.json");
    let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    for transcript in fixture["transcripts"].as_array().unwrap() {
        let (text, tools) = last_turn(transcript["jsonl"].as_str().unwrap());
        assert_eq!(
            (text.as_str(), tools as u64),
            (
                transcript["text"].as_str().unwrap(),
                transcript["tools"].as_u64().unwrap()
            ),
            "{}",
            transcript["name"]
        );
        assert_eq!(
            should_nudge(&text, tools),
            transcript["nudge"] == true,
            "{}",
            transcript["name"]
        );
    }
}
