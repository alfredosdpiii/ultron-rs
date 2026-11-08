//! The judge backends: one call each, no retries of their own (the seat decides on the one retry a `retry-after`
//! allows). Jev and d1 speak System One over [`blocking_http`]; the chat rung is in [`crate::chat`].

use crate::question::{self, Answer, Question};
use blocking_http::{HttpRequest, Transport, TransportError};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// TypeSafe's System One endpoint (Jev).
pub const JEV_URL: &str = "https://api.typesafe.ai/v1/systemone";
/// How long a Jev call may take.
pub const JEV_TIMEOUT: Duration = Duration::from_secs(10);
/// d1's first call of a seat (the model may be loading: measured 6 s cold), and every call after one succeeded.
pub const D1_COLD_TIMEOUT: Duration = Duration::from_secs(15);
pub const D1_WARM_TIMEOUT: Duration = Duration::from_secs(3);
/// The wire format's parts that tell the backends apart: Jev's path ends so and its state is `state`; d1's path is
/// `<url>/decide` and its state is `text`.
const JEV_PATH: &str = "/systemone";
const JEV_STATE: &str = "state";
const D1_PATH: &str = "/decide";
const D1_STATE: &str = "text";

/// A request a seat sent, read back: its backend (`jev` or `d1`), the state it judged and its body (the questions
/// are `body["questions"]`). For transports that record or replay the seat's calls (optimize).
pub fn wire_case(request: &HttpRequest) -> Option<(&'static str, String, Value)> {
    let body: Value = serde_json::from_slice(request.body.as_deref()?).ok()?;
    let (backend, field) = if request.url.ends_with(JEV_PATH) {
        ("jev", JEV_STATE)
    } else if request.url.ends_with(D1_PATH) {
        ("d1", D1_STATE)
    } else {
        return None;
    };
    let state = body[field].as_str()?.to_string();
    Some((backend, state, body))
}

/// A System One answer body as the backends read it ([`reply`]): `answers` by question name, the `usage`, the
/// `model` when known.
pub fn wire_reply(answers: serde_json::Map<String, Value>, usage: &Value, model: Option<&str>) -> Value {
    let mut reply = json!({"answers": answers, "usage": usage});
    if let Some(model) = model {
        reply["model"] = json!(model);
    }
    reply
}

const NO_JEV_KEY: &str = "no TypeSafe key (TYPESAFE_API_KEY or auth.json \"judge:typesafe\")";
const NO_D1_URL: &str = "no d1 server (judge.d1.url)";
const NO_D1_KEY: &str = "no d1 key (ULTRON_D1_KEY or auth.json \"judge:d1\")";
/// The largest answer read.
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// What a call spent. `None` is unknown, never a guessed zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spend {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<f64>,
}

/// A backend's answer to every question, the model that gave it (as the backend names it) and its spend.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub answers: Vec<Answer>,
    pub model: String,
    pub spend: Spend,
}

/// Why a backend gave no answer.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// Not set up (no key, no URL, no model); nothing was sent.
    Unavailable(String),
    /// The seat's call ceiling, budget or the session's usage limits refused the call; nothing was sent.
    Refused(String),
    /// A 429 or 5xx whose `retry-after` allows one more try after the wait.
    Retry {
        status: u16,
        after: Duration,
    },
    /// Sent, and failed: a status, a timeout, no answer, or an answer that does not answer the questions.
    Failed(String),
    Aborted,
}

impl Failure {
    pub fn message(&self) -> String {
        match self {
            Failure::Unavailable(m) | Failure::Refused(m) | Failure::Failed(m) => m.clone(),
            Failure::Retry { status, .. } => format!("HTTP {status}"),
            Failure::Aborted => "aborted".into(),
        }
    }

    /// Whether a request went out (it is a call, with a receipt).
    pub fn sent(&self) -> bool {
        matches!(self, Failure::Retry { .. } | Failure::Failed(_))
    }
}

pub trait Backend: Send + Sync {
    /// `jev`, `d1` or `chat`.
    fn name(&self) -> &'static str;
    /// The model asked for (`jev-latest`, `d1`, `provider/model`).
    fn model(&self) -> String;
    /// Whether its calls cost money (the seat's budget applies).
    fn paid(&self) -> bool;
    /// `Err` (why) when it is not set up, so nothing would be sent.
    fn ready(&self) -> Result<(), String>;
    fn call(&self, state: &str, questions: &[Question], signal: &Signal) -> Result<Reply, Failure>;
}

/// A key for the URL a call goes to, read when the call is made (a key saved during a session applies to its next
/// call; one saved for another server, or a URL that is not https, is refused: [`config::auth::service_key`]).
pub type KeySource = config::auth::ServiceKeySource;

/// The key `source` gives for `url`: `Err` (why) when there is none or it must not go there.
fn key_for(source: &KeySource, url: &str, none: &str) -> Result<String, String> {
    source(url)?.ok_or_else(|| none.to_string())
}

/// `retry-after` as a delay from `now_ms`: whole seconds, or an HTTP date (a past date is no wait;
/// [`ai::http::parse_http_date`], the model providers' reading).
pub fn retry_after(value: Option<&str>, now_ms: i64) -> Option<Duration> {
    let value = jsrt::string::trim(value?);
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = ai::http::parse_http_date(value)?;
    Some(Duration::from_millis((at - now_ms).max(0) as u64))
}

/// POST `body` as JSON with a bearer key; the JSON answer of a 2xx status.
fn post_json(
    transport: &dyn Transport,
    url: &str,
    key: &str,
    body: &Value,
    timeout: Duration,
    signal: &Signal,
) -> Result<Value, Failure> {
    let request = HttpRequest {
        method: "POST".into(),
        url: url.to_string(),
        headers: vec![
            ("authorization".into(), format!("Bearer {key}")),
            ("content-type".into(), "application/json".into()),
        ],
        body: Some(body.to_string().into_bytes()),
    };
    let response = transport
        .send(request, MAX_RESPONSE_BYTES, timeout, signal)
        .map_err(|error| match error {
            TransportError::Aborted => Failure::Aborted,
            TransportError::Timeout => Failure::Failed(format!("timed out after {} s", timeout.as_secs_f64())),
            TransportError::TooLarge => Failure::Failed("answer too large".into()),
            TransportError::Failed => Failure::Failed("no answer (connection failed)".into()),
        })?;
    let status = response.status;
    if !(200..300).contains(&status) {
        return Err(
            match retry_after(response.retry_after.as_deref(), jsrt::date::now_ms()) {
                Some(after) if status == 429 || status >= 500 => Failure::Retry { status, after },
                _ => Failure::Failed(format!("HTTP {status}")),
            },
        );
    }
    serde_json::from_slice(&response.body).map_err(|_| Failure::Failed("answer is not JSON".into()))
}

fn tokens(usage: &Value, key: &str) -> Option<u64> {
    usage.get(key).and_then(Value::as_u64)
}

/// A System One answer: every question answered, the model it names (else `requested`).
fn reply(body: &Value, questions: &[Question], requested: &str, cost: Option<f64>) -> Result<Reply, Failure> {
    let answers = question::wire_answers(questions, &body["answers"]).map_err(Failure::Failed)?;
    let usage = &body["usage"];
    Ok(Reply {
        answers,
        model: body["model"].as_str().unwrap_or(requested).to_string(),
        spend: Spend {
            input_tokens: tokens(usage, "input_tokens"),
            output_tokens: tokens(usage, "output_tokens"),
            cost,
        },
    })
}

/// Jev on TypeSafe: `POST <url> {state, model, questions}`. Its cost is what the answer's `usage.cost` reports,
/// else its reported tokens at `price_per_mtok` (`judge.jev.pricePerMTok`); without either it is unknown.
pub struct Jev {
    pub url: String,
    pub model: String,
    /// USD per million tokens, input and output alike.
    pub price_per_mtok: Option<f64>,
    pub key: KeySource,
    pub transport: Arc<dyn Transport>,
    pub timeout: Duration,
}

impl Backend for Jev {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn model(&self) -> String {
        self.model.clone()
    }

    fn paid(&self) -> bool {
        true
    }

    fn ready(&self) -> Result<(), String> {
        key_for(&self.key, &self.url, NO_JEV_KEY).map(drop)
    }

    fn call(&self, state: &str, questions: &[Question], signal: &Signal) -> Result<Reply, Failure> {
        let key = key_for(&self.key, &self.url, NO_JEV_KEY).map_err(Failure::Unavailable)?;
        let body = json!({JEV_STATE: state, "model": self.model, "questions": question::wire_questions(questions)});
        let answer = post_json(&*self.transport, &self.url, &key, &body, self.timeout, signal)?;
        let usage = &answer["usage"];
        let priced = self
            .price_per_mtok
            .zip(tokens(usage, "input_tokens").zip(tokens(usage, "output_tokens")))
            .map(|(price, (input, output))| (input + output) as f64 * price / 1_000_000.0);
        let cost = usage["cost"].as_f64().filter(|c| c.is_finite() && *c >= 0.0).or(priced);
        reply(&answer, questions, &self.model, cost)
    }
}

/// d1, a self-hosted System One server: `POST <url>/decide {text, questions}`. It costs nothing per call. The first
/// call of a seat waits [`D1_COLD_TIMEOUT`] (it warms the server); once one succeeded, [`D1_WARM_TIMEOUT`], until a
/// call fails, which makes the next one cold again.
pub struct D1 {
    pub url: Option<String>,
    pub key: KeySource,
    pub transport: Arc<dyn Transport>,
    pub warm: AtomicBool,
}

impl D1 {
    pub fn new(url: Option<String>, key: KeySource, transport: Arc<dyn Transport>) -> D1 {
        D1 {
            url,
            key,
            transport,
            warm: AtomicBool::new(false),
        }
    }
}

impl Backend for D1 {
    fn name(&self) -> &'static str {
        "d1"
    }

    fn model(&self) -> String {
        "d1".into()
    }

    fn paid(&self) -> bool {
        false
    }

    fn ready(&self) -> Result<(), String> {
        let url = self.url.as_ref().ok_or(NO_D1_URL)?;
        key_for(&self.key, url, NO_D1_KEY).map(drop)
    }

    fn call(&self, state: &str, questions: &[Question], signal: &Signal) -> Result<Reply, Failure> {
        let url = self
            .url
            .as_ref()
            .ok_or_else(|| Failure::Unavailable(NO_D1_URL.into()))?;
        let key = key_for(&self.key, url, NO_D1_KEY).map_err(Failure::Unavailable)?;
        let timeout = if self.warm.load(Ordering::SeqCst) {
            D1_WARM_TIMEOUT
        } else {
            D1_COLD_TIMEOUT
        };
        let body = json!({D1_STATE: state, "questions": question::wire_questions(questions)});
        let answered = post_json(
            &*self.transport,
            &format!("{url}{D1_PATH}"),
            &key,
            &body,
            timeout,
            signal,
        )
        .and_then(|answer| reply(&answer, questions, "d1", Some(0.0)));
        self.warm.store(answered.is_ok(), Ordering::SeqCst);
        answered
    }
}
