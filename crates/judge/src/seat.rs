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
use kernel::Signal;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Where the seat reserves and settles each call it sends: the session's usage ledger (session_core
/// `session_judge::SessionLedger` over delegation's `UsageLedger`), so judge calls count against the session's limits.
pub trait SpendLedger: Send + Sync {
    /// The ledger's cost cap in USD, if it has one.
    fn cost_cap(&self) -> Option<f64>;
    /// Admits the call `id` (`judge:<call id>`, a `model` call); `Err` (why) when a limit refuses it. The
    /// reservation is what [`SpendLedger::settle`] is given back.
    fn reserve(&self, id: &str) -> Result<Value, String>;
    /// Settles a reserved call: `status` succeeded, failed or cancelled, what it spent and how long it took. A
    /// ledger that cannot record keeps that to itself: the decision stands.
    fn settle(&self, reservation: &Value, status: &str, spend: &Spend, wall_ms: f64);
}

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
    /// Whether the answers stand: a first or escalated answer, not one left for review or an abstention. The one
    /// rule every caller (dreaming, skill procedures, knowledge) reads, as `settled` in [`Verdict::to_json`].
    pub fn settled(&self) -> bool {
        matches!(self.rung, Rung::First | Rung::Escalated)
    }

    /// The receipt of the call whose answers these are.
    pub fn answered_by(&self) -> Option<&Receipt> {
        self.receipts
            .iter()
            .rev()
            .find(|r| r.status == "succeeded" && Some(&r.backend) == self.backend.as_ref())
    }

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
            "settled": self.settled(),
            "receipt": self.answered_by().map(|r| r.id.clone()),
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
    pub ledger: Option<Arc<dyn SpendLedger>>,
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

    /// [`SeatDeps::new`] with the TypeSafe and d1 keys of a profile: the environment `var`, else the auth.json at
    /// `auth_path` (config::auth `TYPESAFE_KEY`, `D1_KEY`), read at each call.
    pub fn with_profile_keys(
        transport: Arc<dyn Transport>,
        var: config::auth::Vars,
        auth_path: std::path::PathBuf,
    ) -> SeatDeps {
        use config::auth::{D1_KEY, TYPESAFE_KEY, service_key_source};
        SeatDeps::new(
            transport,
            service_key_source(TYPESAFE_KEY, var.clone(), auth_path.clone()),
            service_key_source(D1_KEY, var, auth_path),
        )
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
    ledger: Option<Arc<dyn SpendLedger>>,
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
        self.spent.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
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
        if backend.name() == "jev"
            && self.settings.jev_price_per_mtok.is_none()
            && let Some(cap) = self.ledger.as_ref().and_then(|l| l.cost_cap())
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

    fn reserve_ledger(&self, id: &str) -> Result<Option<Value>, Stop> {
        match &self.ledger {
            Some(ledger) => Ok(Some(ledger.reserve(id).map_err(Stop::Refused)?)),
            None => Ok(None),
        }
    }

    fn settle_ledger(&self, reservation: Option<&Value>, receipt: &Receipt) {
        if let (Some(ledger), Some(reservation)) = (&self.ledger, reservation) {
            let status = match receipt.status {
                "succeeded" => "succeeded",
                "failed" => "failed",
                _ => "cancelled",
            };
            ledger.settle(reservation, status, &receipt.spend, receipt.ms);
        }
    }

    fn record_spend(&self, backend: &dyn Backend, receipt: &Receipt) {
        let mut spent = self.spent();
        spent.calls += 1;
        spent.usd += receipt.spend.cost.unwrap_or(0.0);
        if backend.name() == "jev" && receipt.spend.cost.is_none() && self.settings.budget_usd.is_some() {
            spent.jev_unmetered = true;
        }
    }

    /// One backend, with the one retry a `retry-after` allows; every call has a receipt.
    fn attempt(
        &self,
        backend: &dyn Backend,
        ask: &Ask<'_>,
        escalation: bool,
        receipts: &mut Vec<Receipt>,
    ) -> Result<Option<Reply>, Stop> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let id = format!("judge:{}", jsrt::random::uuid_v7(jsrt::date::now_ms()));
            let mut receipt = initial_receipt(&id, backend, escalation, ask.kind);
            if let Err(reason) = self.admit(backend)?.and_then(|()| backend.ready()) {
                receipt.error = Some(reason);
                self.receipt(receipt, receipts);
                return Ok(None);
            }
            let reservation = self.reserve_ledger(&id)?;
            let started = Instant::now();
            let outcome = backend.call(ask.state, ask.questions, ask.signal);
            receipt.ms = started.elapsed().as_secs_f64() * 1000.0;
            let sent = apply_attempt_outcome(&mut receipt, &outcome, backend.paid());
            self.settle_ledger(reservation.as_ref(), &receipt);
            if sent {
                self.record_spend(backend, &receipt);
            }
            self.receipt(receipt, receipts);
            if let Some(done) = finish_attempt(outcome, attempt, ask.signal)? {
                return Ok(done);
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

    fn first_reply<'a>(&'a self, ask: &Ask<'_>, receipts: &mut Vec<Receipt>) -> Result<Option<(&'a str, Reply)>, Stop> {
        for backend in self.backends.iter().chain(self.fallback.iter()) {
            if let Some(reply) = self.attempt(&**backend, ask, false, receipts)? {
                return Ok(Some((backend.name(), reply)));
            }
        }
        Ok(None)
    }

    fn escalation_reason(
        &self,
        backend_name: &str,
        reply: &Reply,
        questions: &[Question],
        kind: Option<&str>,
        expect: &[(String, Value)],
    ) -> Option<String> {
        let disputed = expect.iter().find_map(|(name, held)| {
            let answer = reply.answers.get(questions.iter().position(|q| &q.name == name)?)?;
            (answer.value != *held).then(|| format!("disputed: {name} is {}, the caller holds {held}", answer.value))
        });
        let unsure = self
            .threshold(kind, backend_name)
            .filter(|t| reply.answers.iter().any(|a| a.confidence < *t))
            .map(|t| format!("confidence under {t} ({backend_name})"));
        disputed.or(unsure)
    }

    fn escalate_verdict(
        &self,
        ask: &Ask<'_>,
        backend_name: &str,
        reply: Reply,
        below: String,
        mut receipts: Vec<Receipt>,
    ) -> Verdict {
        let Some(escalate) = &self.escalate else {
            return review_verdict(
                ask.questions,
                backend_name,
                reply,
                format!("{below}; no escalation model (judge.escalateModel)"),
                receipts,
            );
        };
        if family(&escalate.model()) == family(&reply.model) {
            let reason = format!("{below}; the escalation model is of the same family as {}", reply.model);
            return review_verdict(ask.questions, backend_name, reply, reason, receipts);
        }
        match self.attempt(&**escalate, ask, true, &mut receipts) {
            Ok(Some(escalated)) => Verdict {
                answers: name_answers(ask.questions, escalated.answers),
                backend: Some("escalate".into()),
                model: Some(escalated.model),
                rung: Rung::Escalated,
                reason: Some(below),
                receipts,
            },
            Ok(None) | Err(Stop::Refused(_)) => review_verdict(
                ask.questions,
                backend_name,
                reply,
                format!("{below}; the escalation model gave no answer"),
                receipts,
            ),
            Err(Stop::Aborted) => abstain_verdict("aborted".into(), receipts),
        }
    }

    /// [`Seat::decide`] by a caller that already holds an answer (`expect`: question name and value, such as a
    /// verifier's): a first answer that differs is a dispute, escalated like an unsure one. Every backend (Jev, d1,
    /// the fallback and escalation models) sees `state` with its secrets redacted (host::secrets, the rules cell
    /// output is masked with): a quoted `.env` line or a key in a check's output never leaves the machine.
    pub fn decide_expecting(
        &self,
        state: &str,
        questions: &[Question],
        kind: Option<&str>,
        expect: &[(String, Value)],
        signal: &Signal,
    ) -> Verdict {
        let (state, _) = host::secrets::SecretDetector::shipped().redact_counted(state);
        let ask = Ask {
            state: state.as_str(),
            questions,
            kind,
            signal,
        };
        let mut receipts = Vec::new();
        let first = match self.first_reply(&ask, &mut receipts) {
            Ok(first) => first,
            Err(Stop::Aborted) => return abstain_verdict("aborted".into(), receipts),
            Err(Stop::Refused(reason)) => return abstain_verdict(reason, receipts),
        };
        let Some((backend_name, reply)) = first else {
            let reason = if receipts.is_empty() {
                "no judge backend is configured (judge.provider, judge.fallbackModel)"
            } else {
                "no judge backend answered"
            };
            return abstain_verdict(reason.to_string(), receipts);
        };
        let Some(below) = self.escalation_reason(backend_name, &reply, questions, kind, expect) else {
            return Verdict {
                answers: name_answers(questions, reply.answers),
                backend: Some(backend_name.into()),
                model: Some(reply.model),
                rung: Rung::First,
                reason: None,
                receipts,
            };
        };
        self.escalate_verdict(&ask, backend_name, reply, below, receipts)
    }
}

fn initial_receipt(id: &str, backend: &dyn Backend, escalation: bool, kind: Option<&str>) -> Receipt {
    Receipt {
        id: id.to_string(),
        backend: backend.name().into(),
        model: backend.model(),
        status: "skipped",
        escalation,
        kind: kind.map(str::to_string),
        ms: 0.0,
        spend: Spend::default(),
        error: None,
    }
}

fn apply_attempt_outcome(receipt: &mut Receipt, outcome: &Result<Reply, Failure>, paid: bool) -> bool {
    let sent = match outcome {
        Ok(reply) => {
            receipt.status = "succeeded";
            receipt.model = reply.model.clone();
            receipt.spend = reply.spend.clone();
            true
        }
        Err(failure) => {
            let sent = failure.sent();
            receipt.status = match failure {
                Failure::Aborted => "aborted",
                _ if sent => "failed",
                _ => "skipped",
            };
            receipt.error = Some(failure.message());
            sent
        }
    };
    if sent && !paid {
        receipt.spend.cost.get_or_insert(0.0);
    }
    sent
}

fn finish_attempt(
    outcome: Result<Reply, Failure>,
    attempt: u64,
    signal: &Signal,
) -> Result<Option<Option<Reply>>, Stop> {
    match outcome {
        Ok(reply) => Ok(Some(Some(reply))),
        Err(Failure::Aborted) => Err(Stop::Aborted),
        Err(Failure::Retry { after, .. })
            if attempt < RETRY.max_attempts() && after.as_millis() as u64 <= RETRY.max_delay_ms =>
        {
            if signal.wait(after) {
                return Err(Stop::Aborted);
            }
            Ok(None)
        }
        Err(_) => Ok(Some(None)),
    }
}

fn name_answers(questions: &[Question], answers: Vec<Answer>) -> Vec<(String, Answer)> {
    questions.iter().map(|q| q.name.clone()).zip(answers).collect()
}

fn abstain_verdict(reason: String, receipts: Vec<Receipt>) -> Verdict {
    Verdict {
        answers: Vec::new(),
        backend: None,
        model: None,
        rung: Rung::Abstained,
        reason: Some(reason),
        receipts,
    }
}

fn review_verdict(
    questions: &[Question],
    backend_name: &str,
    reply: Reply,
    reason: String,
    receipts: Vec<Receipt>,
) -> Verdict {
    Verdict {
        answers: name_answers(questions, reply.answers),
        backend: Some(backend_name.into()),
        model: Some(reply.model),
        rung: Rung::Review,
        reason: Some(reason),
        receipts,
    }
}
