//! Single ownership of a storage file: one process at a time may write a session's storage. The owner holds an
//! exclusive `flock` on `<file>.owner` for as long as it lives and records itself there (`{pid, acquiredAt}`); the
//! kernel drops the lock when the owner dies, however it dies, so a dead owner's lock is simply taken over (its
//! stale record is overwritten). A second claimant is refused with the live owner's pid (`SessionInUseError`).

use rustix::fs::{FlockOperation, flock};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Why ownership was not taken.
#[derive(Debug)]
pub enum OwnerError {
    /// Another live process owns the file (its recorded pid, when readable).
    InUse {
        pid: Option<u32>,
    },
    Io(std::io::Error),
}

impl std::fmt::Display for OwnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OwnerError::InUse { pid: Some(pid) } => write!(f, "Storage is owned by process {pid}"),
            OwnerError::InUse { pid: None } => f.write_str("Storage is owned by another process"),
            OwnerError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for OwnerError {}

/// The owner record of `target` (`<target>.owner`).
pub fn owner_path(target: &Path) -> PathBuf {
    PathBuf::from(format!("{}.owner", target.display()))
}

/// The pid recorded as `target`'s owner, whether or not it still holds the lock.
pub fn recorded_owner(target: &Path) -> Option<u32> {
    let text = std::fs::read_to_string(owner_path(target)).ok()?;
    let record: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    record["pid"].as_u64().map(|pid| pid as u32)
}

/// Exclusive ownership of one storage file, held until dropped (or the process ends).
#[derive(Debug)]
pub struct OwnerLock {
    file: File,
    path: PathBuf,
}

impl OwnerLock {
    /// Take ownership of `target` for this process, or report who holds it. A new owner records its pid just after
    /// it locks: a refusal waits up to `grace` for that record (named in the refusal; none when it never appears),
    /// and takes the lock instead if the owner lets go meanwhile.
    pub fn acquire(target: &Path, grace: Duration) -> Result<OwnerLock, OwnerError> {
        let path = owner_path(target);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(OwnerError::Io)?;
        let deadline = Instant::now() + grace;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(rustix::io::Errno::WOULDBLOCK) => {
                    let pid = recorded_owner(target);
                    if pid.is_some() || Instant::now() >= deadline {
                        return Err(OwnerError::InUse { pid });
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(OwnerError::Io(error.into())),
            }
        }
        let acquired_at = jsrt::date::now_ms() as u64;
        let record = serde_json::json!({"pid": std::process::id(), "acquiredAt": acquired_at});
        file.set_len(0).map_err(OwnerError::Io)?;
        file.seek(SeekFrom::Start(0)).map_err(OwnerError::Io)?;
        file.write_all(format!("{record}\n").as_bytes())
            .map_err(OwnerError::Io)?;
        file.sync_data().map_err(OwnerError::Io)?;
        Ok(OwnerLock { file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for OwnerLock {
    /// The record goes first, then the lock: a reader never sees this process's pid after it let go.
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}
