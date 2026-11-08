//! Node's names for the platform this build runs on (`process.platform`, `process.arch`), and the PATH lookup of an
//! executable.

/// `process.platform`.
pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

/// `process.arch`.
pub fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    }
}

/// Whether `path` is a regular file a process may execute (`accessSync(path, X_OK)` on a file).
pub fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The first executable `name` on `path` (a `PATH` value; empty entries are skipped); a `name` with a `/` is
/// itself checked.
pub fn which(name: &str, path: &str) -> Option<std::path::PathBuf> {
    if name.contains('/') {
        let own = std::path::PathBuf::from(name);
        return is_executable(&own).then_some(own);
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| std::path::Path::new(dir).join(name))
        .find(|candidate| is_executable(candidate))
}
