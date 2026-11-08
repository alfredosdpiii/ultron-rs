//! The judge seat: who answers, in which order, at what cost, and what happens when the answer is not sure enough.
//!
//! - Fallback: the backends of `judge.provider` in order (Jev, then d1), then the chat rung on `judge.fallbackModel`;
//!   a backend that is not set up, fails, times out, is over the budget, or (under a budget) reports no cost gives
//!   way to the next. When none answers the verdict abstains.
//! - Escalation, separate from fallback: an answer whose confidence is under the threshold of its kind and backend
//!   (`judge.thresholds`; Jev 0.75, d1 0.85 by default, none for the chat rung) goes to `judge.escalateModel` (by
//!   default the cheapest model of the session model's family), which must be of another family than the model
//!   that answered. Without one, or when it fails too, the verdict is `review`: a person settles it.
//! - Spend: every call that went out is reserved and settled in the session's usage ledger (kind `model`, request
//!   key `judge:<call id>`) and reported to `on_receipt`; the seat stops at `judge.maxCalls` calls and, for paid
//!   backends, at `judge.budgetUsd`. Under a budget, Jev is passed over once it answered without a cost; under the
//!   session's cost cap (`max_cost_usd`), which an unpriced call would exhaust, it is not asked at all. A call is never repeated silently: only a 429 or 5xx whose `retry-after` names
//!   a wait (seconds or an HTTP date) within the policy's cap is tried once more, and that try has its own receipt.

use crate::backend::{Backend, D1, Failure, JEV_TIMEOUT, JEV_URL, Jev, KeySource, Reply, Spend};
use crate::chat::{Chat, ChatModel, default_escalate_model, family};
use crate::question::{Answer, Question};
use ai::retry::RetryPolicy;
use blocking_http::Transport;
use config::keys::JudgeSettings;
use delegation::usage::{Measurement, ReserveRequest, UsageLedger};
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Confidence under which a backend's answer is escalated when `judge.thresholds` names none (from the judge
/// benchmark: Jev's wrong answers were all under 0.75, d1's under 0.85).
pub fn default_threshold(backend: &str) -> Option<f64> {
    match backend {
        "jev" => Some(0.75),
        "d1" => Some(0.85),
        _ => None,
    }
}

/// One retry, only after a `retry-after` of at most ten seconds.
pub const RETRY: RetryPolicy = RetryPolicy {
    enabled: true,
    max_retries: 1,
    base_delay_ms: 0,
    max_delay_ms: 10_000,
};

/// Where the verdict came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rung {
    /// The first backend that answered, sure enough.
    First,
    /// The escalation model's answer.
    Escalated,
    /// Not sure enough and nothing could settle it: the answers are the first backend's, for a person to review.
    Review,
    /// No backend answered.
    Abstained,
}

impl Rung {
    pub fn name(self) -> &'static str {
        match self {
            Rung::First => "first",
            Rung::Escalated => "escalated",
            Rung::Review => "review",
            Rung::Abstained => "abstained",
        }
    }
}

/// One backend call, or one backend passed over.
#[derive(Clone, Debug, PartialEq)]
pub struct Receipt {
    /// `judge:<call id>`, the usage ledger's request key.
    pub id: String,
    pub backend: String,
    /// The model asked for, or the one that answered.
    pub model: String,
    /// `succeeded`, `failed` (sent), `skipped` (not set up, over the budget: nothing sent) or `aborted`.
    pub status: &'static str,
    pub escalation: bool,
    pub kind: Option<String>,
    pub ms: f64,
    pub spend: Spend,
    pub error: Option<String>,
}

impl Receipt {
    /// Whether a request went out.
    pub fn sent(&self) -> bool {
        matches!(self.status, "succeeded" | "failed")
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "backend": self.backend,
            "model": self.model,
            "status": self.status,
            "escalation": self.escalation,
            "ms": (self.ms * 10.0).round() / 10.0,
            "input_tokens": self.spend.input_tokens,
            "output_tokens": self.spend.output_tokens,
            "cost_usd": self.spend.cost,
            "error": self.error,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// `(question name, answer)` in the order asked; empty when abstained.
    pub answers: Vec<(String, Answer)>,
    /// The backend and model whose answers these are.
    pub backend: Option<String>,
    pub model: Option<String>,
    pub rung: Rung,
    /// Why the verdict is `review` or abstained.
    pub reason: Option<String>,
    pub receipts: Vec<Receipt>,
}

impl Verdict {
    /// What `judge.decide` returns.
    pub fn to_json(&self, kind: Option<&str>) -> Value {
        let answers: Map<String, Value> = self.answers.iter().map(|(n, a)| (n.clone(), a.to_json())).collect();
        let sent: Vec<&Receipt> = self.receipts.iter().filter(|r| r.sent()).collect();
        let cost = sent
            .iter()
            .filter(|r| r.status == "succeeded")
            .try_fold(0.0, |total, r| r.spend.cost.map(|c| total + c));
        json!({
            "answers": answers,
            "backend": self.backend,
            "model": self.model,
            "rung": self.rung.name(),
            "abstained": self.rung == Rung::Abstained,
            "review": self.rung == Rung::Review,
            "reason": self.reason,
            "kind": kind,
            "calls": sent.len(),
            "cost_usd": cost,
            "receipts": self.receipts.iter().map(Receipt::to_json).collect::<Vec<_>>(),
        })
    }
}

/// Told of every receipt (the session writes a usage row for each call that went out).
pub type OnReceipt = Arc<dyn Fn(&Receipt) + Send + Sync>;

/// What a seat is built from.
pub struct SeatDeps {
    pub transport: Arc<dyn Transport>,
    pub jev_key: KeySource,
    pub d1_key: KeySource,
    /// The chat rung's model access; without it there is no fallback or escalation model.
    pub chat: Option<Arc<dyn ChatModel>>,
    /// The session's `provider/model`: the family the default escalation model comes from.
    pub session_model: Option<String>,
    pub ledger: Option<UsageLedger>,
    pub on_receipt: Option<OnReceipt>,
    /// Jev's endpoint ([`JEV_URL`]; a test server in tests).
    pub jev_url: String,
}

impl SeatDeps {
    pub fn new(transport: Arc<dyn Transport>, jev_key: KeySource, d1_key: KeySource) -> SeatDeps {
        SeatDeps {
            transport,
            jev_key,
            d1_key,
            chat: None,
            session_model: None,
            ledger: None,
            on_receipt: None,
            jev_url: JEV_URL.into(),
        }
    }
}

#[derive(Default)]
struct Spent {
    calls: u64,
    usd: f64,
    /// Jev answered without a cost while a budget is set: it is passed over from then on.
    jev_unmetered: bool,
}

pub struct Seat {
    settings: JudgeSettings,
    backends: Vec<Arc<dyn Backend>>,
    fallback: Option<Arc<dyn Backend>>,
    escalate: Option<Arc<dyn Backend>>,
    ledger: Option<UsageLedger>,
    on_receipt: Option<OnReceipt>,
    spent: Arc<Mutex<Spent>>,
}

/// One question set as every backend is asked it.
#[derive(Clone, Copy)]
struct Ask<'a> {
    state: &'a str,
    questions: &'a [Question],
    kind: Option<&'a str>,
    signal: &'a Signal,
}

/// Why the seat stops asking anyone.
enum Stop {
    Aborted,
    Refused(String),
}

impl Seat {
    pub fn new(settings: JudgeSettings, deps: SeatDeps) -> Seat {
        let backends = settings
            .providers
            .iter()
            .map(|name| -> Arc<dyn Backend> {
                match name.as_str() {
                    "jev" => Arc::new(Jev {
                        url: deps.jev_url.clone(),
                        model: settings.model.clone(),
                        price_per_mtok: settings.jev_price_per_mtok,
                        key: deps.jev_key.clone(),
                        transport: deps.transport.clone(),
                        timeout: JEV_TIMEOUT,
                    }),
                    _ => Arc::new(D1::new(
                        settings.d1_url.clone(),
                        deps.d1_key.clone(),
                        deps.transport.clone(),
                    )),
                }
            })
            .collect();
        let chat = |model: Option<String>| -> Option<Arc<dyn Backend>> {
            Some(Arc::new(Chat {
                model: model?,
                chat: deps.chat.clone()?,
            }))
        };
        let escalate = settings
            .escalate_model
            .clone()
            .or_else(|| default_escalate_model(deps.session_model.as_deref()?));
        Seat {
            fallback: chat(settings.fallback_model.clone()),
            escalate: chat(escalate),
            backends,
            ledger: deps.ledger,
            on_receipt: deps.on_receipt,
            settings,
            spent: Arc::default(),
        }
    }

    /// This seat counting its calls and spend with `earlier`'s (a seat rebuilt for changed settings keeps the
    /// session's ceiling and budget).
    pub fn sharing_spend(mut self, earlier: &Seat) -> Seat {
        self.spent = earlier.spent.clone();
        self
    }

    /// The settings it was built from.
    pub fn settings(&self) -> &JudgeSettings {
        &self.settings
    }

    /// The escalation model, when there is one.
    pub fn escalate_model(&self) -> Option<String> {
        self.escalate.as_ref().map(|b| b.model())
    }

    /// `jev`, `d1`, `fallback` or `escalate`.
    pub fn backend(&self, name: &str) -> Option<Arc<dyn Backend>> {
        match name {
            "fallback" => self.fallback.clone(),
            "escalate" => self.escalate.clone(),
            name => self.backends.iter().find(|b| b.name() == name).cloned(),
        }
    }

    fn spent(&self) -> std::sync::MutexGuard<'_, Spent> {
        self.spent.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The threshold of `kind` for `backend`: `judge.thresholds.<kind>.<backend>`, else `judge.thresholds.<backend>`,
    /// else the default.
    pub fn threshold(&self, kind: Option<&str>, backend: &str) -> Option<f64> {
        let t = &self.settings.thresholds;
        kind.and_then(|k| t.get(k)?.get(backend)?.as_f64())
            .or_else(|| t.get(backend)?.as_f64())
            .or_else(|| default_threshold(backend))
    }

    /// Whether the seat may send `backend` one more call; `Err(Stop)` ends the whole decision.
    fn admit(&self, backend: &dyn Backend) -> Result<Result<(), String>, Stop> {
        let spent = self.spent();
        if spent.calls >= self.settings.max_calls {
            return Err(Stop::Refused(format!(
                "the judge's call ceiling ({} calls, judge.maxCalls) is reached",
                self.settings.max_calls
            )));
        }
        // Jev's answers carry no cost: unless `judge.jev.pricePerMTok` prices their tokens, one settled unpriced in a
        // ledger with a cost cap would stop all of the session's model work ("pricing unknown"), so under that cap
        // an unpriced Jev is passed over.
        if backend.name() == "jev"
            && self.settings.jev_price_per_mtok.is_none()
            && let Some(cap) = self.ledger.as_ref().and_then(|l| l.limits().max_cost_usd)
        {
            return Ok(Err(format!(
                "the session's cost cap (${cap}, max_cost_usd) needs priced calls, and Jev reports no cost (set judge.jev.pricePerMTok)"
            )));
        }
        if let Some(budget) = self.settings.budget_usd.filter(|_| backend.paid()) {
            if spent.usd >= budget {
                return Ok(Err(format!("the judge's budget (${budget}, judge.budgetUsd) is spent")));
            }
            if backend.name() == "jev" && spent.jev_unmetered {
                return Ok(Err("Jev reports no cost, and judge.budgetUsd needs one".into()));
            }
        }
        Ok(Ok(()))
    }

    fn receipt(&self, receipt: Receipt, receipts: &mut Vec<Receipt>) {
        if receipt.sent()
            && let Some(on_receipt) = &self.on_receipt
        {
            on_receipt(&receipt);
        }
        receipts.push(receipt);
    }

    /// One backend, with the one retry a `retry-after` allows; every call has a receipt.
    fn attempt(
        &self,
        backend: &dyn Backend,
        ask: &Ask<'_>,
        escalation: bool,
        receipts: &mut Vec<Receipt>,
    ) -> Result<Option<Reply>, Stop> {
        let Ask {
            state,
            questions,
            kind,
            signal,
        } = *ask;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let id = format!("judge:{}", jsrt::random::uuid_v7(jsrt::date::now_ms()));
            let mut receipt = Receipt {
                id: id.clone(),
                backend: backend.name().into(),
                model: backend.model(),
                status: "skipped",
                escalation,
                kind: kind.map(str::to_string),
                ms: 0.0,
                spend: Spend::default(),
                error: None,
            };
            if let Err(reason) = self.admit(backend)?.and_then(|()| backend.ready()) {
                receipt.error = Some(reason);
                self.receipt(receipt, receipts);
                return Ok(None);
            }
            let reservation = match &self.ledger {
                Some(ledger) => Some(ledger.reserve(&ReserveRequest::model(id)).map_err(Stop::Refused)?),
                None => None,
            };
            let started = Instant::now();
            let outcome = backend.call(state, questions, signal);
            receipt.ms = started.elapsed().as_secs_f64() * 1000.0;
            let sent = match &outcome {
                Ok(_) => true,
                Err(failure) => failure.sent(),
            };
            receipt.status = match &outcome {
                Ok(_) => "succeeded",
                Err(Failure::Aborted) => "aborted",
                Err(failure) if failure.sent() => "failed",
                Err(_) => "skipped",
            };
            if let Ok(reply) = &outcome {
                receipt.model = reply.model.clone();
                receipt.spend = reply.spend.clone();
            }
            if let Err(failure) = &outcome {
                receipt.error = Some(failure.message());
            }
            // A free backend's call costs nothing, answered or not (never an unpriced call in the ledger).
            if sent && !backend.paid() {
                receipt.spend.cost.get_or_insert(0.0);
            }
            if let (Some(ledger), Some(reservation)) = (&self.ledger, &reservation) {
                let status = match receipt.status {
                    "succeeded" => "succeeded",
                    "failed" => "failed",
                    _ => "cancelled",
                };
                let spend = &receipt.spend;
                let total = spend.input_tokens.zip(spend.output_tokens).map(|(i, o)| (i + o) as f64);
                let measured = Measurement {
                    input_tokens: spend.input_tokens.map(|n| n as f64),
                    output_tokens: spend.output_tokens.map(|n| n as f64),
                    total_tokens: total,
                    cost: spend.cost,
                    wall_ms: Some(receipt.ms),
                };
                // A ledger that cannot record is the session's to report; the decision stands.
                let _ = ledger.settle(reservation, Some(status), Some(&measured));
            }
            if sent {
                let mut spent = self.spent();
                spent.calls += 1;
                spent.usd += receipt.spend.cost.unwrap_or(0.0);
                if backend.name() == "jev" && receipt.spend.cost.is_none() && self.settings.budget_usd.is_some() {
                    spent.jev_unmetered = true;
                }
            }
            self.receipt(receipt, receipts);
            match outcome {
                Ok(reply) => return Ok(Some(reply)),
                Err(Failure::Aborted) => return Err(Stop::Aborted),
                Err(Failure::Retry { after, .. })
                    if attempt < RETRY.max_attempts() && after.as_millis() as u64 <= RETRY.max_delay_ms =>
                {
                    if signal.wait(after) {
                        return Err(Stop::Aborted);
                    }
                }
                Err(_) => return Ok(None),
            }
        }
    }

    /// Ask one backend (`jev`, `d1`, `fallback`, `escalate`) directly, under the seat's limits: what `ultron judge
    /// bench` measures.
    pub fn ask(
        &self,
        backend: &str,
        state: &str,
        questions: &[Question],
        signal: &Signal,
    ) -> (Result<Reply, String>, Vec<Receipt>) {
        let mut receipts = Vec::new();
        let Some(backend) = self.backend(backend) else {
            return (Err(format!("{backend} is not configured")), receipts);
        };
        let outcome = match self.attempt(
            &*backend,
            &Ask {
                state,
                questions,
                kind: None,
                signal,
            },
            false,
            &mut receipts,
        ) {
            Ok(Some(reply)) => Ok(reply),
            Ok(None) => Err(receipts
                .last()
                .and_then(|r| r.error.clone())
                .unwrap_or_else(|| "no answer".into())),
            Err(Stop::Aborted) => Err("aborted".into()),
            Err(Stop::Refused(reason)) => Err(reason),
        };
        (outcome, receipts)
    }

    /// Decide `questions` about `state`: fallback, then escalation (see the module docs).
    pub fn decide(&self, state: &str, questions: &[Question], kind: Option<&str>, signal: &Signal) -> Verdict {
        self.decide_expecting(state, questions, kind, &[], signal)
    }

    /// [`Seat::decide`] by a caller that already holds an answer (`expect`: question name and value, such as a
    /// verifier's): a first answer that differs is a dispute, escalated like an unsure one.
    pub fn decide_expecting(
        &self,
        state: &str,
        questions: &[Question],
        kind: Option<&str>,
        expect: &[(String, Value)],
        signal: &Signal,
    ) -> Verdict {
        let mut receipts = Vec::new();
        let abstain = |reason: String, receipts: Vec<Receipt>| Verdict {
            answers: Vec::new(),
            backend: None,
            model: None,
            rung: Rung::Abstained,
            reason: Some(reason),
            receipts,
        };
        let ask = Ask {
            state,
            questions,
            kind,
            signal,
        };
        let mut first: Option<(&Arc<dyn Backend>, Reply)> = None;
        for backend in self.backends.iter().chain(self.fallback.iter()) {
            match self.attempt(&**backend, &ask, false, &mut receipts) {
                Ok(Some(reply)) => {
                    first = Some((backend, reply));
                    break;
                }
                Ok(None) => continue,
                Err(Stop::Aborted) => return abstain("aborted".into(), receipts),
                Err(Stop::Refused(reason)) => return abstain(reason, receipts),
            }
        }
        let Some((backend, reply)) = first else {
            let reason = if receipts.is_empty() {
                "no judge backend is configured (judge.provider, judge.fallbackModel)".to_string()
            } else {
                "no judge backend answered".to_string()
            };
            return abstain(reason, receipts);
        };
        let named = |answers: Vec<Answer>| -> Vec<(String, Answer)> {
            questions.iter().map(|q| q.name.clone()).zip(answers).collect()
        };
        let disputed = expect.iter().find_map(|(name, held)| {
            let answer = reply.answers.get(questions.iter().position(|q| &q.name == name)?)?;
            (answer.value != *held).then(|| format!("disputed: {name} is {}, the caller holds {held}", answer.value))
        });
        let threshold = self.threshold(kind, backend.name());
        let unsure = threshold
            .filter(|t| reply.answers.iter().any(|a| a.confidence < *t))
            .map(|t| format!("confidence under {t} ({})", backend.name()));
        let Some(below) = disputed.or(unsure) else {
            return Verdict {
                answers: named(reply.answers),
                backend: Some(backend.name().into()),
                model: Some(reply.model),
                rung: Rung::First,
                reason: None,
                receipts,
            };
        };
        let review = |reason: String, receipts: Vec<Receipt>, reply: Reply| Verdict {
            answers: named(reply.answers),
            backend: Some(backend.name().into()),
            model: Some(reply.model),
            rung: Rung::Review,
            reason: Some(reason),
            receipts,
        };
        let Some(escalate) = &self.escalate else {
            return review(
                format!("{below}; no escalation model (judge.escalateModel)"),
                receipts,
                reply,
            );
        };
        if family(&escalate.model()) == family(&reply.model) {
            return review(
                format!("{below}; the escalation model is of the same family as {}", reply.model),
                receipts,
                reply,
            );
        }
        match self.attempt(&**escalate, &ask, true, &mut receipts) {
            Ok(Some(escalated)) => Verdict {
                answers: named(escalated.answers),
                backend: Some("escalate".into()),
                model: Some(escalated.model),
                rung: Rung::Escalated,
                reason: Some(below),
                receipts,
            },
            Ok(None) | Err(Stop::Refused(_)) => {
                review(format!("{below}; the escalation model gave no answer"), receipts, reply)
            }
            Err(Stop::Aborted) => abstain("aborted".into(), receipts),
        }
    }
}
