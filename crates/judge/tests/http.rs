//! Jev and d1 over the real transport against loopback servers: the request each sends, the `retry-after` the
//! transport reads, and a backend that does not answer in time.

use crate::fakes::{fake_key, key, serve};
use config::keys::judge_settings;
use judge::question::parse_questions;
use judge::seat::{Rung, Seat, SeatDeps};
use kernel::Signal;
use serde_json::json;
use std::sync::Arc;

fn yes_no() -> Vec<judge::Question> {
    parse_questions(&json!({"q": {"type": "yesno", "instructions": "Shown?"}})).unwrap()
}

#[test]
fn judge_jev_over_http_retries_once_after_retry_after() {
    let (base, seen) = serve(vec![
        (503, vec![("retry-after", "0".into())], String::new()),
        (
            200,
            vec![("content-type", "application/json".into())],
            json!({"answers": {"q": {"noul": 0.91}}, "model": "jev-1.13",
                "usage": {"input_tokens": 400, "output_tokens": 42, "cost": 0.0003}})
            .to_string(),
        ),
    ]);
    let transport = Arc::new(blocking_http::ReqwestTransport::new().unwrap());
    let mut deps = SeatDeps::new(transport, key(Some("jev")), key(None));
    deps.jev_url = format!("{base}/v1/systemone");
    let seat = Seat::new(judge_settings(&serde_json::Map::new()), deps);
    let verdict = seat.decide("state", &yes_no(), None, &Signal::new());
    assert_eq!(
        (verdict.rung, verdict.model.as_deref()),
        (Rung::First, Some("jev-1.13"))
    );
    assert_eq!(verdict.receipts.len(), 2);
    assert_eq!(verdict.receipts[1].spend.cost, Some(0.0003));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, "/v1/systemone");
    assert_eq!(seen[0].1, format!("Bearer {}", fake_key("jev")));
    assert_eq!(seen[1].2["model"], "jev-latest");
    assert_eq!(seen[1].2["questions"]["q"]["type"], "noul");
}

#[test]
fn judge_d1_over_http_posts_to_decide() {
    let (base, seen) = serve(vec![(
        200,
        Vec::new(),
        json!({"answers": {"q": {"noul": 0.02}}, "ms": 194.3}).to_string(),
    )]);
    let transport = Arc::new(blocking_http::ReqwestTransport::new().unwrap());
    let deps = SeatDeps::new(transport, key(None), key(Some("d1")));
    let settings = judge_settings(
        json!({"judge": {"d1": {"url": format!("{base}/")}}})
            .as_object()
            .unwrap(),
    );
    let seat = Seat::new(settings, deps);
    let verdict = seat.decide("text", &yes_no(), None, &Signal::new());
    assert_eq!(
        (verdict.backend.as_deref(), verdict.answers[0].1.value.clone()),
        (Some("d1"), json!(false))
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].0, "/decide");
    assert_eq!(seen[0].1, format!("Bearer {}", fake_key("d1")));
    assert_eq!(seen[0].2["text"], "text");
}

#[test]
fn judge_a_refused_connection_falls_through() {
    // A port nothing listens on: Jev's call fails, there is nothing else, the verdict abstains.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let transport = Arc::new(blocking_http::ReqwestTransport::new().unwrap());
    let mut deps = SeatDeps::new(transport, key(Some("jev")), key(None));
    deps.jev_url = format!("http://127.0.0.1:{port}/v1/systemone");
    let seat = Seat::new(judge_settings(&serde_json::Map::new()), deps);
    let verdict = seat.decide("state", &yes_no(), None, &Signal::new());
    assert_eq!(verdict.rung, Rung::Abstained);
    assert_eq!(verdict.receipts[0].status, "failed");
    assert!(
        verdict.receipts[0]
            .error
            .as_deref()
            .unwrap()
            .contains("connection failed")
    );
}
