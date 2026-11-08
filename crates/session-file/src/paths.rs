//! `normalizePath`/`resolvePath` ([`jsrt::path`]) against this process's `HOME` and cwd, as the session manager
//! reads them.

/// `os.homedir()`.
pub fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".to_string())
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// `path.resolve(path, "..")`: the parent directory of a resolved path.
pub fn parent(path: &str) -> String {
    jsrt::path::resolve(&jsrt::path::resolve(&process_cwd(), path), "..")
}

/// `normalizePath`: expands a leading `~` and converts `file://` URLs to paths.
pub fn normalize_path(input: &str) -> String {
    jsrt::path::normalize_path(input, &home_dir())
}

/// `resolvePath`: `normalizePath`, then `path.resolve` against the process cwd.
pub fn resolve_path(input: &str) -> String {
    jsrt::path::resolve_path(input, &process_cwd(), &home_dir())
}
