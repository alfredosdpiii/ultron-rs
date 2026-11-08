//! Config value resolution for MCP server `env`, `headers` and `oauth.clientSecret` (`core/resolve-config-value.ts`,
//! the uncached `OrThrow` path the MCP runtime uses) over an env overlay, and `~` expansion of stdio paths
//! (`runtime.ts` `expandHome`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Variables come from `overlay` first (empty counts as unset), then the process environment; `overlay` is also added
/// to a `!command`'s environment.
struct Inputs {
    envs: Vec<(String, String)>,
}

impl Inputs {
    fn of(overlay: &HashMap<String, String>) -> Self {
        Inputs {
            envs: overlay.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        self.envs
            .iter()
            .find(|(k, v)| k == name && !v.is_empty())
            .map(|(_, v)| v.clone())
            .or_else(|| std::env::var(name).ok())
    }
}

/// `resolveConfigValueOrThrow` over `overlay`.
pub fn value(config: &str, description: &str, overlay: &HashMap<String, String>) -> Result<String, String> {
    let inputs = Inputs::of(overlay);
    config::auth::resolve_or_err(config, description, &|name| inputs.lookup(name), &inputs.envs)
}

/// `resolveHeadersOrThrow` over `overlay`.
pub fn headers(
    headers: &[(String, String)],
    description: &str,
    overlay: &HashMap<String, String>,
) -> Result<Option<Vec<(String, String)>>, String> {
    let inputs = Inputs::of(overlay);
    config::auth::resolve_headers_or_err(headers, description, &|name| inputs.lookup(name), &inputs.envs)
}

/// `~` and `~/…` name `home`, like in a shell.
pub fn expand_home(value: &str, home: &Path) -> PathBuf {
    if value == "~" {
        return home.to_path_buf();
    }
    match value.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(value),
    }
}
