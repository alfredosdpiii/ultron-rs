//! `RlmPoller`: the one poll the native TUI (answers later, by tag) and `ultron watch` (blocking requests) share.

use rlm_views::poller::{Answered, Inspect, RlmPoller, TUI_FRAME_LIMIT, WATCH_FRAME_LIMIT};
use rlm_views::visualizer::RlmClock;
use serde_json::{Value, json};

fn sent(poller: &RlmPoller) -> Vec<Inspect> {
    let mut out = Vec::new();
    poller.refresh(&mut |inspect| out.push(inspect));
    out
}

#[test]
fn rlm_poller_round_asks_every_inspection_with_its_frame_limit() {
    let round = sent(&RlmPoller::new(TUI_FRAME_LIMIT));
    let asked: Vec<(&str, &str, &Value)> = round.iter().map(|i| (i.tag.as_str(), i.request, &i.payload)).collect();
    assert_eq!(
        asked,
        [
            ("agents", "agents.status", &json!({"graph": true})),
            ("frames", "rlm.frames", &json!({"limit": 200})),
            ("pool", "rlm.pool", &json!({})),
            ("context", "ctx.state", &json!({})),
            ("instances", "instances.list", &json!({})),
        ]
    );
    let watch = sent(&RlmPoller::new(WATCH_FRAME_LIMIT));
    assert_eq!(watch[1].payload, json!({"limit": 20}));
}

#[test]
fn rlm_poller_falls_back_to_the_plain_listing_once() {
    let mut poller = RlmPoller::new(TUI_FRAME_LIMIT);
    let mut again = Vec::new();
    let error = "Unknown payload field graph".to_string();
    assert_eq!(
        poller.answer("agents", Err(error.clone()), &mut |i| again.push(i)),
        Answered::Agents
    );
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].request, &again[0].payload), ("agents.status", &json!({})));
    assert_eq!(poller.error(), Some(error.as_str()));
    assert_eq!(sent(&poller)[0].payload, json!({}));
    // Already plain: another rejection is only shown.
    again.clear();
    poller.answer("agents", Err(error), &mut |i| again.push(i));
    assert!(again.is_empty());
    poller.answer(
        "agents",
        Ok(json!({"tasks": [{"id": "t1", "state": "running"}]})),
        &mut |_| {},
    );
    assert_eq!(poller.error(), None);
    assert_eq!(poller.status().unwrap().tasks.len(), 1);
}

#[test]
fn rlm_poller_progress_round_replaces_assessments_when_complete() {
    let mut poller = RlmPoller::new(TUI_FRAME_LIMIT);
    let ids: Vec<String> = (0..10).map(|i| format!("t{i}")).collect();
    let mut asked = Vec::new();
    poller.assess(&ids, &mut |i| asked.push(i));
    assert_eq!(asked.len(), 8);
    assert_eq!(asked[0].tag, "progress:1:t0");
    assert_eq!(asked[0].payload, json!({"task_id": "t0"}));
    let assessed = json!({"classification": "advancing", "receipts": []});
    assert!(poller_snapshot(&poller).progress.is_empty());
    for (n, inspect) in asked.iter().enumerate() {
        let result = if n == 1 {
            Err("gone".to_string())
        } else {
            Ok(assessed.clone())
        };
        let answered = poller.answer(&inspect.tag, result, &mut |_| {});
        assert_eq!(answered, if n == 7 { Answered::Progress } else { Answered::Other });
    }
    let progress = poller_snapshot(&poller).progress;
    assert_eq!(progress.len(), 7);
    assert!(!progress.contains_key("t1"));
    // A stale round's answer changes nothing; an empty round clears.
    assert_eq!(
        poller.answer("progress:1:t0", Ok(assessed), &mut |_| {}),
        Answered::Other
    );
    poller.assess(&[], &mut |_| panic!("nothing to assess"));
    assert!(poller_snapshot(&poller).progress.is_empty());
}

fn poller_snapshot(poller: &RlmPoller) -> rlm_views::visualizer::RlmSnapshot {
    let status = poller.status().cloned().unwrap_or_default();
    poller.snapshot(status, &mut RlmClock::new(), 1000.0, None, Vec::new(), None)
}

#[test]
fn rlm_poller_poll_runs_a_blocking_round_and_keeps_answers_across_failures() {
    let mut poller = RlmPoller::new(WATCH_FRAME_LIMIT);
    let mut asked = Vec::new();
    poller.poll(&mut |request, payload| {
        asked.push(request.to_string());
        match request {
            "agents.status" => Ok(json!({"tasks": [{"id": "t1", "state": "running"}]})),
            "rlm.frames" => {
                assert_eq!(payload, json!({"limit": 20}));
                Ok(json!({"frames": [{"id": "frame-1", "status": "completed"}]}))
            }
            "rlm.pool" => Ok(json!({"live": 1, "maxLive": 4})),
            _ => Err("unknown".into()),
        }
    });
    assert_eq!(
        asked,
        ["agents.status", "rlm.frames", "rlm.pool", "ctx.state", "instances.list"]
    );
    let snapshot = poller_snapshot(&poller);
    assert_eq!(snapshot.tasks.len(), 1);
    assert_eq!(snapshot.frames.len(), 1);
    assert!(snapshot.pool.is_some());
    assert!(snapshot.context.is_none());
    // Everything fails: the listing and frames stay, the pool becomes unknown, the error shows.
    poller.poll(&mut |_, _| Err("socket closed".into()));
    let snapshot = poller_snapshot(&poller);
    assert_eq!((snapshot.tasks.len(), snapshot.frames.len()), (1, 1));
    assert!(snapshot.pool.is_none());
    assert_eq!(snapshot.error.as_deref(), Some("socket closed"));
}
