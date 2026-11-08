//! The judge seat: closed-set decisions (choice, yes/no, score, number) asked of a small decision model, for batch
//! and background work such as dreaming over memories, checking review findings or classifying relations. It never
//! sits on a turn's own path: the model asks it on purpose (`judge.decide` in the REPL) and `ultron judge bench`
//! measures it.
//!
//! Jev (TypeSafe) answers first, a self-hosted d1 server when Jev cannot, then `judge.fallbackModel`; an answer
//! that is not sure enough is escalated to a model of another family, else marked for review ([`seat`]). Requests
//! use TypeSafe's System One format, which d1 shares ([`question`]); HTTP goes through [`blocking_http`]; spend is
//! settled in the session's usage ledger.

pub mod backend;
pub mod bench;
pub mod chat;
pub mod module;
pub mod question;
pub mod seat;

pub use backend::{Backend, D1, Failure, Jev, Reply, Spend};
pub use chat::{Chat, ChatModel, DispatchChat};
pub use module::JudgeModule;
pub use question::{Answer, Kind, Question};
pub use seat::{Receipt, Rung, Seat, SeatDeps, SpendLedger, Verdict};
