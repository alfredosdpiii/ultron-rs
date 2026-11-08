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

/// `sessionFileName`.
pub fn session_file_name(created_at: i64, id: &str) -> String {
    let timestamp = jsrt::date::iso_string(created_at).replace([':', '.'], "-");
    format!("{timestamp}_{}.jsonl", jsrt::uri::encode_component(id))
}

/// `path.resolve(p)` against the process cwd, without resolving symlinks.
pub fn absolute(path: &str) -> String {
    let base = std::env::current_dir().unwrap_or_default();
    jsrt::path::resolve(&base.to_string_lossy(), path)
}

/// What a file's first line says it is, as the selectors and readers that take any file check it (loosely: a
/// native header is any object with `kind: "header"`, a Pi header any object with `type: "session"`).
pub enum HeaderLine {
    Native(Map<String, Value>),
    Pi(Map<String, Value>),
    /// JSON, but neither.
    Other,
    NotJson,
}

impl HeaderLine {
    pub fn parse(line: &str) -> HeaderLine {
        match serde_json::from_str::<Value>(line) {
            Err(_) => HeaderLine::NotJson,
            Ok(Value::Object(map)) if map.get("kind").and_then(Value::as_str) == Some("header") => {
                HeaderLine::Native(map)
            }
            Ok(Value::Object(map)) if map.get("type").and_then(Value::as_str) == Some("session") => HeaderLine::Pi(map),
            Ok(_) => HeaderLine::Other,
        }
    }

    /// The first line of `path`.
    pub fn read(path: &Path) -> std::io::Result<HeaderLine> {
        io::first_line(path).map(|(line, _)| HeaderLine::parse(&line))
    }

    /// The header of a native session file.
    pub fn native(self) -> Option<Map<String, Value>> {
        match self {
            HeaderLine::Native(header) => Some(header),
            _ => None,
        }
    }
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
    let (line, _) = io::first_line(path).ok()?;
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

/// The session `id` under `root` (`resolveSession` of the server): none, or an error when more than one file has it.
pub fn find(root: &Path, id: &str) -> Result<Option<Metadata>, String> {
    let files = session_files(root);
    let mut found = files
        .iter()
        .filter_map(|path| read_metadata(path))
        .filter(|metadata| metadata.id == id);
    match (found.next(), found.next()) {
        (Some(_), Some(_)) => Err(format!(
            "Session {id} is ambiguous: more than one session file has this id"
        )),
        (only, _) => Ok(only),
    }
}

/// Read a session file's transactions without opening the session (its worker may be writing it): every line after
/// the header (as `readline` reads them, so a last line still being written does not parse), unless `skip(line)` (a
/// cheap filter on the raw text), decoded as its writes with legacy names renamed. `each` gets `None` for a line that
/// is not JSON.
pub fn scan_writes(
    path: &Path,
    skip: impl Fn(&str) -> bool,
    mut each: impl FnMut(Option<Vec<Value>>),
) -> std::io::Result<()> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut buffer = Vec::new();
    let mut header = true;
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            return Ok(());
        }
        if std::mem::take(&mut header) {
            continue;
        }
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() || skip(line) {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            each(None);
            continue;
        };
        let mut writes = match parsed {
            Value::Array(items) => items,
            other => vec![other],
        };
        writes.iter_mut().for_each(codec::rename_legacy);
        each(Some(writes));
    }
}
