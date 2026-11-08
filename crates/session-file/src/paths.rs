//! `normalizePath`/`resolvePath` ([`jsrt::path`]) against an [`Env`]'s home directory and cwd, as the session manager
//! reads them.

use crate::Env;

/// `path.resolve(path, "..")`: the parent directory of a resolved path.
pub(crate) fn parent(env: &dyn Env, path: &str) -> String {
    jsrt::path::resolve(&jsrt::path::resolve(&env.cwd(), path), "..")
}

/// `normalizePath`: expands a leading `~` and converts `file://` URLs to paths.
pub(crate) fn normalize_path(env: &dyn Env, input: &str) -> String {
    jsrt::path::normalize_path(input, &env.home())
}

/// `resolvePath`: `normalizePath`, then `path.resolve` against the cwd.
pub(crate) fn resolve_path(env: &dyn Env, input: &str) -> String {
    jsrt::path::resolve_path(input, &env.cwd(), &env.home())
}
