//! Node's names for the platform this build runs on (`process.platform`, `process.arch`) and for signals, and the
//! PATH lookup of an executable.

pub fn platform() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    }
}

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

/// Node's name for a Linux signal number (what a child's `exit` event carries); the number itself when it has none.
pub fn signal_name(number: i32) -> String {
    const NAMES: &[&str] = &[
        "SIGHUP",
        "SIGINT",
        "SIGQUIT",
        "SIGILL",
        "SIGTRAP",
        "SIGABRT",
        "SIGBUS",
        "SIGFPE",
        "SIGKILL",
        "SIGUSR1",
        "SIGSEGV",
        "SIGUSR2",
        "SIGPIPE",
        "SIGALRM",
        "SIGTERM",
        "SIGSTKFLT",
        "SIGCHLD",
        "SIGCONT",
        "SIGSTOP",
        "SIGTSTP",
        "SIGTTIN",
        "SIGTTOU",
        "SIGURG",
        "SIGXCPU",
        "SIGXFSZ",
        "SIGVTALRM",
        "SIGPROF",
        "SIGWINCH",
        "SIGIO",
        "SIGPWR",
        "SIGSYS",
    ];
    usize::try_from(number - 1)
        .ok()
        .and_then(|index| NAMES.get(index))
        .map_or_else(|| number.to_string(), |name| (*name).to_string())
}

#[cfg(test)]
mod tests {
    use super::{signal_name, which};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn jsrt_signal_names_follow_node() {
        assert_eq!(signal_name(3), "SIGQUIT");
        assert_eq!(signal_name(15), "SIGTERM");
        assert_eq!(signal_name(31), "SIGSYS");
        assert_eq!(signal_name(64), "64");
    }

    #[test]
    fn jsrt_which_takes_the_first_executable_file_on_the_path() {
        let root = std::env::temp_dir().join(format!("jsrt-which-{}", std::process::id()));
        let [dir_entry, not_executable, second, first] = ["a", "b", "c", "d"].map(|d| root.join(d));
        for d in [&dir_entry, &not_executable, &second, &first] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir_all(dir_entry.join("tool")).unwrap();
        std::fs::write(not_executable.join("tool"), "").unwrap();
        for d in [&second, &first] {
            std::fs::write(d.join("tool"), "").unwrap();
            std::fs::set_permissions(d.join("tool"), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([&dir_entry, &not_executable, &second, &first]).unwrap();
        let path = path.to_str().unwrap();
        assert_eq!(which("tool", path), Some(second.join("tool")));
        assert_eq!(
            which("tool", &format!("::{path}")),
            Some(second.join("tool")),
            "empty entries are skipped"
        );
        assert_eq!(
            which("tool", dir_entry.to_str().unwrap()),
            None,
            "a directory is not an executable"
        );
        assert_eq!(
            which("tool", not_executable.to_str().unwrap()),
            None,
            "a file without x is not one"
        );
        assert_eq!(which("tool", ""), None);
        let own = first.join("tool");
        assert_eq!(
            which(own.to_str().unwrap(), ""),
            Some(own.clone()),
            "a name with a slash is checked itself"
        );
        assert_eq!(which(not_executable.join("tool").to_str().unwrap(), path), None);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
