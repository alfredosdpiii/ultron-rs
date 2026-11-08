//! Clock lines and their formats (`ultron/clock-context.ts`, `ultron-clock-context.test.ts`).

use serde_json::{Value, json};
use session_core::lane::{clock_lines, format_clock, format_gap, with_clock_lines};

fn utc(ms: i64) -> String {
    format_clock(ms, &jsrt::localtime::Zone::utc())
}

#[test]
fn session_core_format_clock_in_utc() {
    // formatClock(1791208239000) with TZ=UTC; and the epoch, and a leap day.
    assert_eq!(utc(1_791_208_239_000), "2026-10-05 13:50 UTC+00:00");
    assert_eq!(utc(0), "1970-01-01 00:00 UTC+00:00");
    assert_eq!(utc(951_782_400_000), "2000-02-29 00:00 UTC+00:00");
}

#[test]
fn session_core_clock_lines_match_with_clock_lines() {
    // formatGap and withClockLines (packages/coding-agent/src/ultron/clock-context.ts), values from node.
    for (ms, want) in [
        (600_000, "10 min"),
        (7_199_999, "119 min"),
        (7_200_000, "2 h"),
        (172_799_999, "47 h"),
        (172_800_000, "2 d"),
    ] {
        assert_eq!(format_gap(ms), want, "{ms}");
    }
    let user = |t: i64| serde_json::json!({"role": "user", "content": "x", "timestamp": t});
    let assistant = |t: i64| serde_json::json!({"role": "assistant", "content": [], "timestamp": t});
    // Only user messages get a line; a tool result's time still counts as the previous message (no lines here).
    let tool = serde_json::json!({"role": "toolResult", "content": [], "timestamp": 20_000_000});
    assert!(clock_lines(&[user(0), tool, user(20_000_001)]).is_empty());
    let messages = [
        user(0),
        user(1_000),
        assistant(500_000),
        user(1_099_999),
        user(1_700_000),
        user(9_000_000),
    ];
    let lines: Vec<(usize, String)> = clock_lines(&messages)
        .into_iter()
        .map(|(i, m)| match m {
            ai::Message::User(u) => match u.content {
                ai::UserMessageContent::Blocks(b) => match &b[0] {
                    ai::UserContent::Text(t) => (i, t.text.clone()),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            },
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        lines,
        [
            (1, "[clock] 1970-01-01 00:00 UTC+00:00".to_string()),
            (
                4,
                "[clock] 1970-01-01 00:28 UTC+00:00, 10 min after the previous message".to_string()
            ),
            (
                5,
                "[clock] 1970-01-01 02:30 UTC+00:00, 2 h after the previous message".to_string()
            ),
        ]
    );
}

/// `Date.UTC(2026, 9, 5, 12, 0)`.
const T0: i64 = 1_791_201_600_000;

fn user(text: &str, timestamp: i64) -> Value {
    json!({"role": "user", "content": text, "timestamp": timestamp})
}

fn answer(timestamp: i64) -> Value {
    json!({"role": "assistant", "content": [{"type": "text", "text": "ok"}], "timestamp": timestamp})
}

#[test]
fn session_core_clock_lines_open_conversations_and_follow_gaps() {
    let messages = [
        user("a", T0),
        answer(T0 + 1000),
        user("b", T0 + 5 * 60_000),
        answer(T0 + 6 * 60_000),
        user("c", T0 + 3 * 3_600_000),
    ];
    let out = with_clock_lines(&messages).unwrap();
    let text = |m: &Value| m["content"][0]["text"].as_str().map(str::to_string);
    let lines: Vec<String> = out
        .iter()
        .filter_map(text)
        .filter(|t| t.starts_with("[clock] "))
        .collect();
    assert_eq!(
        lines,
        [
            "[clock] 2026-10-05 12:00 UTC+00:00".to_string(),
            "[clock] 2026-10-05 15:00 UTC+00:00, 2 h after the previous message".to_string(),
        ]
    );
    assert_eq!(out.len(), 7);
    assert_eq!(out[6], messages[4]);
    assert_eq!(out[0]["timestamp"], T0);
    assert_eq!(with_clock_lines(&messages), Some(out));
    assert_eq!(with_clock_lines(&[user("x", 0)]), None);
    // Exactly the gap counts; one millisecond short does not.
    assert!(with_clock_lines(&[answer(T0), user("y", T0 + 600_000)]).is_some());
    assert!(with_clock_lines(&[answer(T0), user("y", T0 + 599_999)]).is_none());
}

#[test]
fn session_core_clock_formats() {
    assert_eq!(utc(T0), "2026-10-05 12:00 UTC+00:00");
    assert_eq!(format_gap(12 * 60_000), "12 min");
    assert_eq!(format_gap(119 * 60_000), "119 min");
    assert_eq!(format_gap(3 * 3_600_000), "3 h");
    assert_eq!(format_gap(47 * 3_600_000), "47 h");
    assert_eq!(format_gap(50 * 3_600_000), "2 d");
}
