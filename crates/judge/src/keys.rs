//! The judge services' keys: the environment (`TYPESAFE_API_KEY`, `ULTRON_D1_KEY`, through `ai::env`), else the
//! profile's auth.json entry (`typesafe`, `d1`: an `api_key` credential, its `$VAR` or `!command` resolved as every
//! auth.json key is). Read when a call is made; never logged.

use crate::backend::KeySource;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// TypeSafe (Jev): its auth.json entry and `ai::env` provider name.
pub const TYPESAFE: &str = "typesafe";
/// A d1 server.
pub const D1: &str = "d1";

/// The environment's lookup.
pub type Vars = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// `service`'s key now: its environment variable, else its auth.json `api_key` credential.
pub fn service_key(service: &str, var: &dyn Fn(&str) -> Option<String>, auth_path: &Path) -> Option<String> {
    let lookup = |name: &str| var(name).filter(|v| !v.trim().is_empty());
    let source = ai::env::EnvSource {
        lookup: &lookup,
        exists: &|_| false,
        home: None,
    };
    if let Some(key) = ai::env::env_api_key_in(service, &source) {
        return Some(key);
    }
    let stored = config::auth::AuthStorage::create(auth_path).read(service).ok()??;
    (stored["type"] == "api_key")
        .then(|| stored["key"].as_str().map(str::to_string))
        .flatten()
        .filter(|key| !key.trim().is_empty())
}

/// [`service_key`], read at each call.
pub fn key_source(service: &'static str, var: Vars, auth_path: PathBuf) -> KeySource {
    Arc::new(move || service_key(service, &*var, &auth_path))
}

/// Save `key` as `service`'s auth.json `api_key` credential (the setup wizard).
pub fn save_key(auth_path: &Path, service: &str, key: &str) -> Result<(), String> {
    config::auth::AuthStorage::create(auth_path)
        .modify(service, |_| {
            Some(serde_json::json!({"type": "api_key", "key": key.trim()}))
        })
        .map(|_| ())
}
