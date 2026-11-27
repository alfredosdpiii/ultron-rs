//! The config profile backup (`ultron migrate backup|restore`): the agent directory copied into
//! `<backup dir>/ultron-profile-<timestamp>/` with a manifest of every file (size, sha256, mode) and symlink, sessions,
//! traces, installed binaries and backups left out; a restore verifies the backup against its manifest first.

use serde_json::{Value, json};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

pub const PROFILE_BACKUP_FORMAT: &str = "ultron-profile-backup";
const MANIFEST_FILE: &str = "manifest.json";
const FILES_DIR: &str = "files";
const EXCLUDED_TOP_LEVEL: [&str; 6] = ["sessions", "traces", "bin", "npm", "node_modules", "backups"];

fn io_error(e: &std::io::Error, call: &str, path: &Path) -> String {
    jsrt::fs::error_message(e, call, path)
}

fn is_excluded(relative: &str) -> bool {
    let segments: Vec<&str> = relative.split('/').collect();
    if segments.len() == 1 && EXCLUDED_TOP_LEVEL.contains(&segments[0]) {
        return true;
    }
    let name = segments.last().copied().unwrap_or_default();
    name == "sessions" || name.ends_with("-sessions") || name.ends_with(".lock")
}

fn is_inside(parent: &Path, child: &Path) -> bool {
    child.starts_with(parent)
}

#[derive(PartialEq)]
enum Kind {
    File,
    Symlink,
}

fn sorted_names(dir: &Path) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map_err(|e| io_error(&e, "scandir", dir))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    jsrt::utf16::sort(&mut names);
    Ok(names)
}

fn relative_join(rel_dir: &str, name: String) -> String {
    if rel_dir.is_empty() {
        name
    } else {
        format!("{rel_dir}/{name}")
    }
}

fn visit_profile_entry(
    root: &Path,
    rel: String,
    skip: Option<&Path>,
    out: &mut Vec<(String, Kind)>,
) -> Result<(), String> {
    let absolute = root.join(&rel);
    if is_excluded(&rel) || skip.is_some_and(|s| is_inside(s, &absolute)) {
        return Ok(());
    }
    let meta = std::fs::symlink_metadata(&absolute).map_err(|e| io_error(&e, "lstat", &absolute))?;
    if meta.file_type().is_symlink() {
        out.push((rel, Kind::Symlink));
    } else if meta.is_dir() {
        walk_profile(root, &rel, skip, out)?;
    } else if meta.is_file() {
        out.push((rel, Kind::File));
    }
    Ok(())
}

fn walk_profile(root: &Path, rel_dir: &str, skip: Option<&Path>, out: &mut Vec<(String, Kind)>) -> Result<(), String> {
    for name in sorted_names(&root.join(rel_dir))? {
        let rel = relative_join(rel_dir, name);
        visit_profile_entry(root, rel, skip, out)?;
    }
    Ok(())
}

fn collect_profile(agent_dir: &Path, skip: Option<&Path>) -> Result<Vec<(String, Kind)>, String> {
    let mut out = Vec::new();
    walk_profile(agent_dir, "", skip, &mut out)?;
    Ok(out)
}

fn chmod(path: &Path, mode: u32) -> Result<(), String> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| io_error(&e, "chmod", path))
}

fn mkdir_private(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| io_error(&e, "mkdir", path))?;
    chmod(path, 0o700)
}

/// `mkdir(path, { recursive: true, mode: 0o700 })`: missing levels are created 0700 (less the umask).
fn mkdir_recursive(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| io_error(&e, "mkdir", path))
}

fn write_new(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(|e| io_error(&e, "open", path))?;
    file.write_all(data).map_err(|e| io_error(&e, "write", path))
}

fn create_unique_backup_dir(backup_dir: &Path, base: &str) -> Result<PathBuf, String> {
    for attempt in 0..100 {
        let name = if attempt == 0 {
            base.to_string()
        } else {
            format!("{base}-{attempt}")
        };
        let candidate = backup_dir.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                chmod(&candidate, 0o700)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_error(&e, "mkdir", &candidate)),
        }
    }
    Err(format!(
        "Could not create a unique backup directory under {}",
        backup_dir.display()
    ))
}

fn ensure_parent_dirs(files_root: &Path, rel: &str) -> Result<(), String> {
    let mut current = files_root.to_path_buf();
    for segment in Path::new(rel).parent().into_iter().flat_map(Path::components) {
        current.push(segment);
        if !current.exists() {
            std::fs::create_dir(&current).map_err(|e| io_error(&e, "mkdir", &current))?;
        }
        chmod(&current, 0o700)?;
    }
    Ok(())
}

fn backup_entry(agent_dir: &Path, files_root: &Path, rel: &str, kind: Kind) -> Result<Value, String> {
    let source = agent_dir.join(rel);
    let destination = files_root.join(rel);
    ensure_parent_dirs(files_root, rel)?;
    if kind == Kind::Symlink {
        let link = std::fs::read_link(&source).map_err(|e| io_error(&e, "readlink", &source))?;
        symlink(&link, &destination).map_err(|e| io_error(&e, "symlink", &destination))?;
        return Ok(json!({"path": rel, "type": "symlink", "target": link.to_string_lossy()}));
    }
    let data = std::fs::read(&source).map_err(|e| io_error(&e, "open", &source))?;
    let mode = std::fs::symlink_metadata(&source)
        .map_err(|e| io_error(&e, "lstat", &source))?
        .permissions()
        .mode()
        & 0o777;
    write_new(&destination, &data, 0o600)?;
    chmod(&destination, 0o600)?;
    Ok(json!({
        "path": rel,
        "type": "file",
        "size": data.len(),
        "sha256": jsrt::hash::sha256_hex(&data),
        "mode": mode,
    }))
}

fn populate_backup(agent_dir: &Path, backup_dir: &Path, target: &Path, created: &str) -> Result<usize, String> {
    let files_root = target.join(FILES_DIR);
    mkdir_private(&files_root)?;
    let skip = is_inside(agent_dir, backup_dir).then_some(backup_dir);
    let mut entries = Vec::new();
    for (rel, kind) in collect_profile(agent_dir, skip)? {
        entries.push(backup_entry(agent_dir, &files_root, &rel, kind)?);
    }
    let count = entries.len();
    let manifest = json!({
        "format": PROFILE_BACKUP_FORMAT,
        "version": 1,
        "createdAt": created,
        "sourceAgentDir": agent_dir.display().to_string(),
        "entries": entries,
    });
    write_new(
        &target.join(MANIFEST_FILE),
        format!("{}\n", jsrt::json::stringify_pretty(&manifest)).as_bytes(),
        0o600,
    )?;
    Ok(count)
}

/// Copy the config profile into `<backup_dir>/ultron-profile-<timestamp>/`; returns the path and the entry count.
pub fn backup(agent_dir: &Path, backup_dir: &Path) -> Result<(PathBuf, usize), String> {
    if !std::fs::symlink_metadata(agent_dir).is_ok_and(|m| m.is_dir()) {
        return Err(format!("Profile directory does not exist: {}", agent_dir.display()));
    }
    mkdir_recursive(backup_dir)?;
    let created = jsrt::date::iso_string(jsrt::date::now_ms());
    let base = format!("ultron-profile-{}", created.replace([':', '.'], "-"));
    let target = create_unique_backup_dir(backup_dir, &base)?;
    match populate_backup(agent_dir, backup_dir, &target, &created) {
        Ok(count) => Ok((target, count)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&target);
            Err(e)
        }
    }
}

fn verification_error(problems: &[String]) -> String {
    format!(
        "Profile backup failed verification:\n{}",
        problems
            .iter()
            .map(|p| format!("  - {p}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn is_safe_relative_path(path: Option<&Value>) -> bool {
    let Some(path) = path.and_then(Value::as_str) else {
        return false;
    };
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains('\0')
        && !path.starts_with('/')
        && path.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

fn list_backup_files(root: &Path, rel_dir: &str, out: &mut Vec<String>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root.join(rel_dir))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let rel = if rel_dir.is_empty() {
            name
        } else {
            format!("{rel_dir}/{name}")
        };
        if entry.file_type()?.is_dir() {
            list_backup_files(root, &rel, out)?;
        } else {
            out.push(rel);
        }
    }
    Ok(())
}

fn read_manifest_entries(root: &Path) -> Result<Vec<Value>, String> {
    let manifest_path = root.join(MANIFEST_FILE);
    let manifest: Value = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|e| verification_error(&[format!("unreadable manifest: {e}")]))?
        }
        Err(e) => {
            return Err(verification_error(&[format!(
                "unreadable manifest: {}",
                jsrt::fs::error_message(&e, "open", &manifest_path)
            )]));
        }
    };
    if manifest.get("format").and_then(Value::as_str) != Some(PROFILE_BACKUP_FORMAT)
        || manifest.get("version").and_then(Value::as_f64) != Some(1.0)
    {
        return Err(verification_error(&["unsupported manifest format or version".into()]));
    }
    let Some(entries) = manifest.get("entries").and_then(Value::as_array) else {
        return Err(verification_error(&["manifest has no entries list".into()]));
    };
    Ok(entries.clone())
}

fn verify_symlink_entry(
    absolute: &Path,
    meta: &std::fs::Metadata,
    entry: &Value,
    path: &str,
    problems: &mut Vec<String>,
) {
    let target = std::fs::read_link(absolute).ok();
    if !meta.file_type().is_symlink()
        || target.map(|t| t.to_string_lossy().into_owned()).as_deref() != entry.get("target").and_then(Value::as_str)
    {
        problems.push(format!("symlink changed: {path}"));
    }
}

fn verify_file_entry(absolute: &Path, meta: &std::fs::Metadata, entry: &Value, path: &str, problems: &mut Vec<String>) {
    if !meta.is_file() {
        problems.push(format!("not a regular file: {path}"));
        return;
    }
    let data = std::fs::read(absolute).unwrap_or_default();
    if Some(jsrt::hash::sha256_hex(&data).as_str()) != entry.get("sha256").and_then(Value::as_str)
        || Some(meta.len() as f64) != entry.get("size").and_then(Value::as_f64)
    {
        problems.push(format!("hash mismatch: {path}"));
    }
}

fn verify_manifest_entry(
    files_root: &Path,
    entry: &Value,
    expected: &mut std::collections::HashSet<String>,
    problems: &mut Vec<String>,
) {
    if !is_safe_relative_path(entry.get("path")) {
        let shown = entry.get("path").map_or("undefined".to_string(), jsrt::json::stringify);
        problems.push(format!("unsafe path in manifest: {shown}"));
        return;
    }
    let path = entry["path"].as_str().unwrap_or_default();
    expected.insert(path.to_string());
    let absolute = files_root.join(path);
    let Ok(meta) = std::fs::symlink_metadata(&absolute) else {
        problems.push(format!("missing: {path}"));
        return;
    };
    match entry.get("type").and_then(Value::as_str) {
        Some("symlink") => verify_symlink_entry(&absolute, &meta, entry, path, problems),
        Some("file") => verify_file_entry(&absolute, &meta, entry, path, problems),
        _ => problems.push(format!("unknown entry type for {path}")),
    }
}

/// Verify a backup against its manifest; returns the manifest's entries.
fn verify_profile_backup(root: &Path) -> Result<Vec<Value>, String> {
    let entries = read_manifest_entries(root)?;
    let files_root = root.join(FILES_DIR);
    let mut problems = Vec::new();
    let mut expected = std::collections::HashSet::new();
    for entry in &entries {
        verify_manifest_entry(&files_root, entry, &mut expected, &mut problems);
    }
    let mut present = Vec::new();
    if list_backup_files(&files_root, "", &mut present).is_err() {
        present.clear();
    }
    for path in present {
        if !expected.contains(&path) {
            problems.push(format!("not in manifest: {path}"));
        }
    }
    if !problems.is_empty() {
        return Err(verification_error(&problems));
    }
    Ok(entries)
}

fn stage_restore_entry(files_root: &Path, entry: &Value, rel: &str, temporary: &Path) -> Result<(), String> {
    if entry["type"].as_str() == Some("symlink") {
        let target = entry["target"].as_str().unwrap_or_default();
        return symlink(target, temporary).map_err(|e| io_error(&e, "symlink", temporary));
    }
    let source = files_root.join(rel);
    let data = std::fs::read(&source).map_err(|e| io_error(&e, "open", &source))?;
    if Some(jsrt::hash::sha256_hex(&data).as_str()) != entry["sha256"].as_str() {
        return Err(verification_error(&[format!("hash mismatch: {rel}")]));
    }
    write_new(temporary, &data, 0o600)?;
    chmod(temporary, (entry["mode"].as_f64().unwrap_or(0.0) as u32) & 0o777)
}

fn restore_entry(files_root: &Path, agent_dir: &Path, entry: &Value) -> Result<(), String> {
    let rel = entry["path"].as_str().unwrap_or_default();
    let destination = agent_dir.join(rel);
    if let Some(parent) = destination.parent() {
        mkdir_recursive(parent)?;
    }
    let temporary = PathBuf::from(format!(
        "{}.ultron-restore-{}",
        destination.display(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);
    stage_restore_entry(files_root, entry, rel, &temporary)?;
    if std::fs::symlink_metadata(&destination).is_ok_and(|m| m.is_dir()) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Cannot restore {rel}: a directory is in the way"));
    }
    std::fs::rename(&temporary, &destination).map_err(|e| io_error(&e, "rename", &temporary))
}

/// Restore a verified backup into `agent_dir`; returns how many entries were written.
pub fn restore(root: &Path, agent_dir: &Path) -> Result<usize, String> {
    let entries = verify_profile_backup(root)?;
    let files_root = root.join(FILES_DIR);
    mkdir_recursive(agent_dir)?;
    let mut restored = 0;
    for entry in &entries {
        restore_entry(&files_root, agent_dir, entry)?;
        restored += 1;
    }
    Ok(restored)
}
