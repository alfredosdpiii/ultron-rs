//! Ultron's Hindsight memory client (`createHindsightBackend` of `ultron/memory.ts`), the backend of `memory.*` that
//! the session composition root uses by default (and autoreview's lessons, in crates/review), and the resolution of
//! the server and bank every Ultron instance shares. HTTP goes through [`Transport`] ([`blocking_http`]); [`ReqwestTransport`] is real.

mod backend;
pub mod resolve;

pub use backend::{HindsightBackend, HindsightOptions};
pub use blocking_http::{HttpRequest, HttpResponse, ReqwestTransport, Transport, TransportError};
pub use resolve::{DEFAULT_HINDSIGHT_URL, Endpoint, hindsight_url, worker_scope_tags};
