//! Node's `fs` error messages (`ENOENT: no such file or directory, open '<path>'`) and shared atomic file writes.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The message of a failed `syscall` (`open`, `stat`, ...) on `path`. A directory read as a file fails as Node's
/// `read` does; other errors keep the OS's text with Node's `, <syscall> '<path>'` suffix.
pub fn error_message(error: &io::Error, syscall: &str, path: &Path) -> String {
    let path = path.display();
    match error.kind() {
        io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, {syscall} '{path}'"),
        io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{path}'"),
        io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
        _ => format!("{error}, {syscall} '{path}'"),
    }
}

fn unique_temp_path(path: &Path) -> PathBuf {
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    path.with_file_name(format!(".{name}.{pid}.{seq}.tmp"))
}

/// Atomically writes `content` to `path` via a unique sibling temporary file and rename, creating missing parent
/// directories and removing the temporary file if writing or renaming fails.
pub fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temp = unique_temp_path(path);
    let result = std::fs::write(&temp, content.as_ref()).and_then(|()| std::fs::rename(&temp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Atomically writes `content` to `path` with explicit Unix file permissions `mode` (e.g. `0o600`), creating missing
/// parent directories and cleaning up the temporary file on error.
pub fn atomic_write_mode(path: &Path, content: impl AsRef<[u8]>, mode: u32) -> io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let temp = unique_temp_path(path);
    let result = (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&temp)?;
            file.write_all(content.as_ref())?;
            drop(file);
            std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
            std::fs::write(&temp, content.as_ref())?;
        }
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "jsrt-fs-test-{}-{}",
                std::process::id(),
                crate::random::uuid_v4()
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn atomic_write_creates_parents_and_replaces_content() {
        let dir = TempDir::new();
        let target = dir.0.join("nested/state.json");
        atomic_write(&target, b"first").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        atomic_write(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        let entries: Vec<_> = std::fs::read_dir(target.parent().unwrap()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "no temporary files left behind");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_mode_applies_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let target = dir.0.join("secret.json");
        atomic_write_mode(&target, b"secret", 0o600).unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
