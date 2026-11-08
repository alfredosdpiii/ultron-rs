//! `judge.decide(state, questions, kind=None)` from the REPL: one decision through the session's seat, asked on
//! purpose by the model's code (never on a turn's own path). `expect` names answers the caller already holds: a first
//! answer that differs is a dispute, escalated to another family like an unsure one. The seat follows the global `judge.*` settings as they
//! are at each call, keeping its call and spend counts across a change.

use crate::question::{Question, parse_questions};
use crate::seat::{Seat, Verdict};
use config::keys::JudgeSettings;
use kernel_apis::{ApiModule, Request};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// The longest `state`, in characters.
pub const MAX_STATE_CHARS: usize = 100_000;

/// The settings as they are now.
pub type SettingsSource = Arc<dyn Fn() -> JudgeSettings + Send + Sync>;
/// A seat for the given settings (`Err`: its HTTP client could not be built).
pub type SeatFactory = Arc<dyn Fn(JudgeSettings) -> Result<Seat, String> + Send + Sync>;

pub struct JudgeModule {
    settings: SettingsSource,
    build: SeatFactory,
    seat: Mutex<Option<Arc<Seat>>>,
}

impl JudgeModule {
    pub fn new(settings: SettingsSource, build: SeatFactory) -> JudgeModule {
        JudgeModule {
            settings,
            build,
            seat: Mutex::new(None),
        }
    }

    /// The seat for the settings now: the same one while they are unchanged.
    fn seat(&self) -> Result<Arc<Seat>, String> {
        let settings = (self.settings)();
        let mut slot = self.seat.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(seat) = slot.as_ref().filter(|s| *s.settings() == settings) {
            return Ok(seat.clone());
        }
        let fresh = (self.build)(settings)?;
        let fresh = Arc::new(match slot.as_ref() {
            Some(earlier) => fresh.sharing_spend(earlier),
            None => fresh,
        });
        *slot = Some(fresh.clone());
        Ok(fresh)
    }

    fn decide(&self, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("judge.decide", request.payload)?;
        let state = match request.payload.get("state") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Null) | None => return Err("judge.decide state must be a string or JSON value".into()),
            Some(value) => value.to_string(),
        };
        if jsrt::string::trim(&state).is_empty() {
            return Err("judge.decide state must not be empty".into());
        }
        if state.chars().count() > MAX_STATE_CHARS {
            return Err(format!("judge.decide state is over {MAX_STATE_CHARS} characters"));
        }
        let kind = match request.payload.get("kind") {
            None | Some(Value::Null) => None,
            Some(Value::String(kind)) => Some(kind.as_str()),
            Some(_) => return Err("judge.decide kind must be a short lowercase name (a-z, 0-9, _ . -)".into()),
        };
        let expect = match request.payload.get("expect") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(held)) => held.iter().map(|(name, value)| (name.clone(), value.clone())).collect(),
            Some(_) => return Err("judge.decide expect must be {question: answer}".into()),
        };
        self.decide_json(
            &state,
            request.payload.get("questions").unwrap_or(&Value::Null),
            kind,
            &expect,
            request.signal,
        )
    }

    /// One decision as `judge.decide` answers it (also asked by the host, such as dreaming's checks); `expect`
    /// holds the caller's own answers, and an answer that disputes one is escalated like an unsure one.
    pub fn decide_json(
        &self,
        state: &str,
        questions: &Value,
        kind: Option<&str>,
        expect: &[(String, Value)],
        signal: &kernel::Signal,
    ) -> Result<Value, String> {
        let questions = parse_questions(questions)?;
        if kind.is_some_and(|kind| !valid_kind(kind)) {
            return Err("judge.decide kind must be a short lowercase name (a-z, 0-9, _ . -)".into());
        }
        if let Some((name, _)) = expect
            .iter()
            .find(|(name, _)| !questions.iter().any(|q| &q.name == name))
        {
            return Err(format!("judge.decide expect names no question: {name}"));
        }
        Ok(self.decide_now(state, &questions, kind, expect, signal)?.to_json(kind))
    }

    /// One decision through the seat of the settings now, for host code off a turn's path (a borderline Loki write):
    /// what `judge.decide` serves, without its argument checks.
    pub fn decide_now(
        &self,
        state: &str,
        questions: &[Question],
        kind: Option<&str>,
        expect: &[(String, Value)],
        signal: &kernel::Signal,
    ) -> Result<Verdict, String> {
        Ok(self.seat()?.decide_expecting(state, questions, kind, expect, signal))
    }
}

/// `^[a-z][a-z0-9_.-]{0,63}$`
fn valid_kind(kind: &str) -> bool {
    let bytes = kind.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(b))
}

impl ApiModule for JudgeModule {
    fn prefixes(&self) -> &[&str] {
        &["judge."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        match request.kind {
            "judge.decide" => self.decide(request),
            other => Err(format!("Unknown judge request: {other}")),
        }
    }
}
