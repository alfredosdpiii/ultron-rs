use std::path::PathBuf;

/// A logical server reached at a Unix socket path: its coordinator's public socket, or one generation's endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub server_id: String,
    pub path: PathBuf,
}
