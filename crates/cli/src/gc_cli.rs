//! `ultron gc`: reclaim local state under `<agent_dir>` (orphaned snapshot temp files, old snapshots, spill dirs,
//! and stale session journals).

use crate::Env;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

const DEFAULT_DAYS: u64 = 30;
const ORPHAN_GRACE: Duration = Duration::from_secs(300);

const HELP: &str = "ultron gc — reclaim stale local state under the Ultron profile directory

Usage:
  ultron gc [--dry-run] [--days <N>]

Options:
  --dry-run      Report what would be removed without deleting files
  --days <N>     Remove snapshots, spill dirs, and session logs older than N days (default: 30)
  -h, --help     Show this help
";

#[derive(Debug, Default, PartialEq, Eq)]
struct GcSummary {
    orphan_files: usize,
    orphan_bytes: u64,
    stale_dirs: usize,
    stale_dir_bytes: u64,
    stale_sessions: usize,
    stale_session_bytes: u64,
}

impl GcSummary {
    fn total_bytes(&self) -> u64 {
        self.orphan_bytes
            .saturating_add(self.stale_dir_bytes)
            .saturating_add(self.stale_session_bytes)
    }
}

pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let mut dry_run = false;
    let mut days = DEFAULT_DAYS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                let _ = write!(out, "{HELP}");
                return Ok(0);
            }
            "--dry-run" => dry_run = true,
            "--days" => {
                let Some(value) = args.get(i + 1) else {
                    let _ = writeln!(err, "ultron gc: --days requires a non-negative integer");
                    return Ok(2);
                };
                match value.parse::<u64>() {
                    Ok(parsed) => days = parsed,
                    Err(_) => {
                        let _ = writeln!(err, "ultron gc: invalid --days value: {value}");
                        return Ok(2);
                    }
                }
                i += 1;
            }
            other => {
                let _ = writeln!(err, "ultron gc: unknown argument: {other}");
                return Ok(2);
            }
        }
        i += 1;
    }

    let agent_dir = env.agent_dir();
    let max_age = Duration::from_secs(days.saturating_mul(86_400));
    let summary = collect(&agent_dir, max_age, ORPHAN_GRACE, dry_run);
    let verb = if dry_run { "would remove" } else { "removed" };
    let _ = writeln!(
        out,
        "ultron gc ({agent}): {verb} {orphans} orphan temp files ({ob}), {dirs} stale session dirs ({db}), {sess} old session logs ({sb}) — total {total}",
        agent = agent_dir.display(),
        orphans = summary.orphan_files,
        ob = format_bytes(summary.orphan_bytes),
        dirs = summary.stale_dirs,
        db = format_bytes(summary.stale_dir_bytes),
        sess = summary.stale_sessions,
        sb = format_bytes(summary.stale_session_bytes),
        total = format_bytes(summary.total_bytes()),
    );
    Ok(0)
}

fn collect(agent_dir: &Path, max_age: Duration, orphan_grace: Duration, dry_run: bool) -> GcSummary {
    let mut summary = GcSummary::default();
    let now = SystemTime::now();
    let stale_cutoff = now.checked_sub(max_age);
    let orphan_cutoff = now.checked_sub(orphan_grace);

    let snapshots_root = agent_dir.join("rlm-snapshots");
    sweep_orphans(&snapshots_root, orphan_cutoff, dry_run, &mut summary);

    for bucket in ["rlm-snapshots", "rlm-output", "rlm-tmp"] {
        let root = agent_dir.join(bucket);
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let path = entry.path();
            let (newest, bytes) = dir_stats(&path);
            let is_stale = match (stale_cutoff, newest) {
                (Some(cutoff), Some(mtime)) => mtime <= cutoff,
                (Some(_), None) => true,
                _ => false,
            };
            if is_stale && (dry_run || std::fs::remove_dir_all(&path).is_ok()) {
                summary.stale_dirs += 1;
                summary.stale_dir_bytes = summary.stale_dir_bytes.saturating_add(bytes);
            }
        }
    }

    let sessions_root = agent_dir.join("sessions");
    sweep_stale_sessions(&sessions_root, stale_cutoff, dry_run, &mut summary);

    summary
}

fn sweep_orphans(root: &Path, cutoff: Option<SystemTime>, dry_run: bool, summary: &mut GcSummary) {
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                dirs.push(entry.path());
            }
        }
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !kernel::is_orphan_snapshot_temp(name) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            if let (Some(cutoff), Ok(modified)) = (cutoff, meta.modified())
                && modified > cutoff
            {
                continue;
            }
            if dry_run || std::fs::remove_file(entry.path()).is_ok() {
                summary.orphan_files += 1;
                summary.orphan_bytes = summary.orphan_bytes.saturating_add(meta.len());
            }
        }
    }
}

fn sweep_stale_sessions(dir: &Path, cutoff: Option<SystemTime>, dry_run: bool, summary: &mut GcSummary) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            sweep_stale_sessions(&path, cutoff, dry_run, summary);
            continue;
        }
        if !ft.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let is_stale = match (cutoff, meta.modified().ok()) {
            (Some(cutoff), Some(modified)) => modified <= cutoff,
            _ => false,
        };
        if is_stale && (dry_run || std::fs::remove_file(&path).is_ok()) {
            summary.stale_sessions += 1;
            summary.stale_session_bytes = summary.stale_session_bytes.saturating_add(meta.len());
        }
    }
}

fn dir_stats(dir: &Path) -> (Option<SystemTime>, u64) {
    let mut newest: Option<SystemTime> = std::fs::metadata(dir).ok().and_then(|m| m.modified().ok());
    let mut bytes = 0u64;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (newest, bytes);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(ft) = entry.file_type() else {
            continue;
        };
        if ft.is_dir() {
            let (child_newest, child_bytes) = dir_stats(&path);
            bytes = bytes.saturating_add(child_bytes);
            if let Some(cm) = child_newest
                && newest.is_none_or(|cur| cm > cur)
            {
                newest = Some(cm);
            }
        } else if let Ok(meta) = entry.metadata() {
            bytes = bytes.saturating_add(meta.len());
            if let Ok(m) = meta.modified()
                && newest.is_none_or(|cur| m > cur)
            {
                newest = Some(m);
            }
        }
    }
    (newest, bytes)
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_dry_run_and_delete_handle_orphans_and_age() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path();
        let snap_dir = agent.join("rlm-snapshots/sess-1");
        let sess_dir = agent.join("sessions/proj");
        std::fs::create_dir_all(&snap_dir).unwrap();
        std::fs::create_dir_all(&sess_dir).unwrap();
        let orphan = snap_dir.join(".main.snapshot.abc.unsigned");
        let keep = snap_dir.join("main.snapshot");
        let log = sess_dir.join("old.jsonl");
        std::fs::write(&orphan, b"orphan-data").unwrap();
        std::fs::write(&keep, b"real-snapshot").unwrap();
        std::fs::write(&log, br#"{"type":"session"}"#).unwrap();

        let dry = collect(agent, Duration::from_secs(30 * 86_400), Duration::ZERO, true);
        assert_eq!(dry.orphan_files, 1);
        assert_eq!(dry.orphan_bytes, 11);
        assert_eq!(dry.stale_dirs, 0);
        assert_eq!(dry.stale_sessions, 0);
        assert!(orphan.exists());

        let live = collect(agent, Duration::from_secs(30 * 86_400), Duration::ZERO, false);
        assert_eq!(live.orphan_files, 1);
        assert!(!orphan.exists());
        assert!(keep.exists());
        assert!(log.exists());

        let all = collect(agent, Duration::ZERO, Duration::ZERO, false);
        assert_eq!(all.stale_dirs, 1);
        assert_eq!(all.stale_sessions, 1);
        assert!(!snap_dir.exists());
        assert!(!log.exists());
    }
}
