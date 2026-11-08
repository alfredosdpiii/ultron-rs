//! Node's `fs` error messages (`ENOENT: no such file or directory, open '<path>'`), for the errors a port reports as
//! Node would.

use std::path::Path;

/// The message of a failed `syscall` (`open`, `stat`, ...) on `path`.
pub fn error_message(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, {syscall} '{}'", path.display()),
        std::io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{}'", path.display()),
        _ => error.to_string(),
    }
}
