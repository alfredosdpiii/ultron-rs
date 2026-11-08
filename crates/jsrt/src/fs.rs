//! Node's `fs` error messages (`ENOENT: no such file or directory, open '<path>'`), for the errors a port reports as
//! Node would.

use std::path::Path;

/// The message of a failed `syscall` (`open`, `stat`, ...) on `path`. A directory read as a file fails as Node's
/// `read` does; other errors keep the OS's text with Node's `, <syscall> '<path>'` suffix.
pub fn error_message(error: &std::io::Error, syscall: &str, path: &Path) -> String {
    let path = path.display();
    match error.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, {syscall} '{path}'"),
        std::io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {syscall} '{path}'"),
        std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
        _ => format!("{error}, {syscall} '{path}'"),
    }
}
