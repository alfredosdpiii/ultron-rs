//! Where sessions live and how they are found (`jsonl/repo.ts`): under a sessions root, one directory per cwd
//! (`--<cwd with separators as dashes>--`), one file per session (`<createdAt ISO with ':' and '.' as '-'>_<id>.jsonl`)
//! whose first line is its header. Every reader of a sessions root lists, finds and reads headers through here.

use crate::codec::{self, ParsedHeader};
use crate::io;
use crate::legacy_v3::normalize_header;
use crate::repo::Metadata;
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// `sessionDirectoryName`.
pub fn session_directory_name(cwd: &str) -> String {
    let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
    format!("--{}--", trimmed.replace(['/', '\\', ':'], "-"))
}

/// `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `sessionFileName`.
pub fn session_file_name(created_at: i64, id: &str) -> String {
    let timestamp = jsrt::date::iso_string(created_at).replace([':', '.'], "-");
    format!("{timestamp}_{}.jsonl", encode_uri_component(id))
}

/// `path.resolve(p)` against the process cwd, without resolving symlinks.
pub fn absolute(path: &str) -> String {
    let base = std::env::current_dir().unwrap_or_default();
    config::paths::normalize(&base.join(path)).display().to_string()
}

/// A file's first line without its newline (the whole file when it has none), read no further than that.
pub fn first_line(path: &Path) -> std::io::Result<String> {
    let mut line = Vec::new();
    BufReader::new(std::fs::File::open(path)?).read_until(b'\n', &mut line)?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

pub(crate) fn metadata_from_header(header: &Map<String, Value>, path: &Path, modified_at: f64) -> Metadata {
    Metadata {
        id: codec::header_str(header, "id").unwrap_or("").into(),
        created_at: codec::header_i64(header, "createdAt").unwrap_or(0),
        storage_version: codec::header_i64(header, "storageVersion").unwrap_or(1),
        cwd: codec::header_str(header, "cwd").unwrap_or("").into(),
        path: path.to_path_buf(),
        modified_at,
        parent_session_id: codec::header_str(header, "parentSessionId").map(str::to_string),
        legacy_parent_session_path: codec::header_str(header, "legacyParentSessionPath").map(str::to_string),
    }
}

/// The metadata of one session file from its header line (format 4, or a legacy Pi v3 header as format 4 reads it),
/// or `None` when it is not a session.
pub fn read_metadata(path: &Path) -> Option<Metadata> {
    let line = first_line(path).ok()?;
    let modified = io::mtime_ms(path).unwrap_or(0.0);
    match codec::parse_header(&line).ok()? {
        ParsedHeader::V4(header) => Some(metadata_from_header(&header, path, modified)),
        ParsedHeader::V3(header) => Some(metadata_from_header(&normalize_header(&header), path, modified)),
    }
}

/// The per-cwd directories under `root`.
pub(crate) fn session_directories(root: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

/// The `.jsonl` files of one session directory.
pub(crate) fn directory_files(directory: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(directory)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    e.file_type().is_ok_and(|t| t.is_file()) && e.file_name().to_string_lossy().ends_with(".jsonl")
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

/// Every session file under `root` (`<root>/<dir>/<file>.jsonl`), in no particular order; headers are not read.
pub fn session_files(root: &Path) -> Vec<PathBuf> {
    session_directories(root)
        .iter()
        .flat_map(|d| directory_files(d))
        .collect()
}

/// Most recently modified first, then newest `createdAt`, then id (`listNativeSessions`'s order).
pub fn sort_newest(sessions: &mut [Metadata]) {
    sessions.sort_by(|l, r| {
        r.modified_at
            .total_cmp(&l.modified_at)
            .then(r.created_at.cmp(&l.created_at))
            .then_with(|| jsrt::collate::locale_compare(&l.id, &r.id))
    });
}

/// The sessions under `root` started in `cwd` (every cwd when `None`), most recently modified first. Every directory
/// is read: the directory name is lossy (`/a/b` and `/a-b` share one), so the header's cwd decides.
pub fn list(root: &Path, cwd: Option<&str>) -> Vec<Metadata> {
    let cwd = cwd.map(absolute);
    let mut found: Vec<Metadata> = session_files(root)
        .iter()
        .filter_map(|path| read_metadata(path))
        .filter(|metadata| cwd.as_ref().is_none_or(|c| *c == metadata.cwd))
        .collect();
    sort_newest(&mut found);
    found
}

/// The session `id` under `root`, if one file has it.
pub fn find(root: &Path, id: &str) -> Option<Metadata> {
    session_files(root)
        .iter()
        .filter_map(|path| read_metadata(path))
        .find(|metadata| metadata.id == id)
}
