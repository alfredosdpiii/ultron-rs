//! Ultron's memorantado memory client: the backend of `memory.*`, automatic memory and dreaming when memorantado is
//! the memory server ([`resolve`]), with claim curation (list, confirm, supersede, conflicts) and file-snapshot
//! evidence that Hindsight lacks. HTTP goes through [`blocking_http`]; [`ReqwestTransport`] is real.

mod backend;
pub mod graph;
pub mod resolve;

pub use backend::{BRIEF_WRITER, MemorantadoBackend, MemorantadoOptions};
pub use blocking_http::{HttpRequest, HttpResponse, ReqwestTransport, Transport, TransportError};
pub use graph::{MemoryGraph, MemoryNode};
pub use resolve::{Backend, Choice, DEFAULT_MEMORANTADO_URL, Endpoint, Hindsight};
