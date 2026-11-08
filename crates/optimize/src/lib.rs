//! `ultron optimize <pipeline>`: Ultron's own recurring pipelines measured as versioned variants against frozen test
//! sets, so a cheaper, faster or better configuration is found by measurement and kept with its evidence.
//!
//! - [`judge`]: the judge seat on the judgebench cases. Offline (the default) the seat answers from a frozen
//!   recording of a live run; `--live` records a new one under a budget. Thresholds are fitted per question kind and
//!   written as a proposal, never as settings.
//! - [`memory`]: scoped recall on memorantado's `bench:ultron` set through the `MemoryBackend` trait. Offline it runs
//!   on an in-process reference backend; `--live` on a throwaway bank of the configured server.
//! - [`autoreview`]: recorded `ultron autoreview eval` reports re-scored at each `blockAt` level (a report is a live
//!   run; re-scoring spends nothing).
//!
//! Every run of a variant is a [`point::Point`]: quality, a penalty, cost, and the failures behind them, judged
//! against the pipeline's failure surface (fixed here, before any search). Points and variants are kept in a
//! [`store::Store`]; only points on the same set hash compare.

pub mod autoreview;
pub mod judge;
pub mod memory;
pub mod point;
pub mod store;

pub use point::{Outcome, Point, frontier};
pub use store::Store;
