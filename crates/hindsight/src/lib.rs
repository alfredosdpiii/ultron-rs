//! Ultron's Hindsight memory client (`createHindsightBackend` of `ultron/memory.ts`), the backend of `memory.*` that
//! the session composition root uses by default (and autoreview's lessons, in crates/review), and the resolution of
//! the server and bank every Ultron instance shares. HTTP goes through [`transport::Transport`]; [`ReqwestTransport`] is real.

mod backend;
pub mod resolve;
mod transport;

pub use backend::{HindsightBackend, HindsightOptions, encode_uri_component};
pub use resolve::{DEFAULT_HINDSIGHT_URL, Endpoint, hindsight_url, worker_scope_tags};
pub use transport::{HttpRequest, HttpResponse, ReqwestTransport, Transport, TransportError};
