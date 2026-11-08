//! Indexing one revision of a repository: list its tree, read only the blobs the store has not seen, record the
//! tree. A second index of a nearby revision reads a handful of blobs.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use crate::extract::{EXTRACTOR_VERSION, Extractor, MAX_BYTES, language};
use crate::relations::{Kind, Pruned, RELATIONS_VERSION, prune, scanned};
use crate::store::{Error, Store};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// The commit indexed (the full id).
    pub rev: String,
    /// Indexable files in the tree.
    pub files: usize,
    /// Blobs read this time; the others were already in the store.
    pub read: usize,
    pub ms: u128,
}

fn git(repo_dir: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error(format!("git {}: {error}", args.join(" "))))?;
    if !output.status.success() {
        return Err(Error(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

/// (path, blob, size) of every file at `rev`.
fn blobs_at(repo_dir: &Path, rev: &str) -> Result<Vec<(String, String, u64)>, Error> {
    let listing = git(repo_dir, &["ls-tree", "-r", "-l", "-z", "--full-tree", rev])?;
    let mut entries = Vec::new();
    for record in listing.split(|&byte| byte == 0).filter(|record| !record.is_empty()) {
        let record = String::from_utf8_lossy(record);
        // `<mode> SP <type> SP <object> SP+ <size> TAB <path>`
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        let fields: Vec<&str> = meta.split_whitespace().collect();
        let [_mode, kind, oid, size] = fields[..] else { continue };
        let Ok(size) = size.parse::<u64>() else { continue };
        if kind == "blob" {
            entries.push((path.to_string(), oid.to_string(), size));
        }
    }
    Ok(entries)
}

/// (path, blob, size) of the indexable files at `rev`.
fn tree_entries(repo_dir: &Path, rev: &str) -> Result<Vec<(String, String, u64)>, Error> {
    let mut entries = blobs_at(repo_dir, rev)?;
    entries.retain(|(path, _, size)| language(path).is_some() && *size <= MAX_BYTES);
    Ok(entries)
}

/// The contents of `oids`, in order, through one `git cat-file --batch`.
fn read_blobs(repo_dir: &Path, oids: &[String]) -> Result<Vec<Vec<u8>>, Error> {
    if oids.is_empty() {
        return Ok(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| Error(format!("git cat-file: {error}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| Error("git cat-file: no stdin".into()))?;
    let input = oids.join("\n") + "\n";
    // Written from a thread: git answers while it reads, and a full pipe either way would deadlock.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error("git cat-file: no stdout".into()))?;
    let mut reader = BufReader::new(stdout);
    let mut contents = Vec::with_capacity(oids.len());
    for oid in oids {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|error| Error(format!("git cat-file: {error}")))?;
        let fields: Vec<&str> = header.split_whitespace().collect();
        let size = match fields[..] {
            [_, "blob", size] => size
                .parse::<usize>()
                .map_err(|_| Error(format!("git cat-file: {header:?}")))?,
            _ => return Err(Error(format!("git cat-file: {oid}: {}", header.trim()))),
        };
        let mut body = vec![0; size + 1];
        reader
            .read_exact(&mut body)
            .map_err(|error| Error(format!("git cat-file: {error}")))?;
        body.truncate(size);
        contents.push(body);
    }
    let _ = writer.join();
    let _ = child.wait();
    Ok(contents)
}

/// Indexes `repo` (a stable name such as `github.com/owner/name`) at `rev` of the git checkout `repo_dir` into the
/// store at `db`. Indexing the same revision again only refreshes its tree.
pub fn index_tree(db: &Path, repo: &str, repo_dir: &Path, rev: &str) -> Result<IndexStats, Error> {
    let started = Instant::now();
    if rev.is_empty() || rev.starts_with('-') {
        return Err(Error(format!("not a revision: {rev:?}")));
    }
    let commit = git(
        repo_dir,
        &["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")],
    )?;
    let commit = String::from_utf8_lossy(&commit).trim().to_string();
    let entries = tree_entries(repo_dir, &commit)?;
    let mut store = Store::open(db)?;
    let oids: Vec<&str> = entries.iter().map(|(_, oid, _)| oid.as_str()).collect();
    let known = store.known_blobs(&oids, EXTRACTOR_VERSION)?;
    let mut unread: Vec<(String, String)> = Vec::new();
    let mut queued = std::collections::HashSet::new();
    for (path, oid, _) in &entries {
        if !known.contains(oid) && queued.insert(oid.clone()) {
            unread.push((path.clone(), oid.clone()));
        }
    }
    let mut extractor = Extractor::new().map_err(Error)?;
    let mut blobs = Vec::with_capacity(unread.len());
    let unread_oids: Vec<String> = unread.iter().map(|(_, oid)| oid.clone()).collect();
    for ((path, oid), body) in unread.iter().zip(read_blobs(repo_dir, &unread_oids)?) {
        let Some(lang) = language(path) else { continue };
        // A file that is not UTF-8 is not source anyone reads; it is stored with nothing extracted.
        let extracted = match String::from_utf8(body) {
            Ok(text) => extractor.extract(lang, &text),
            Err(_) => Default::default(),
        };
        blobs.push((oid.clone(), lang.name().to_string(), extracted));
    }
    let paths: Vec<(String, String)> = entries.into_iter().map(|(path, oid, _)| (path, oid)).collect();
    let indexed_at = jsrt::date::now_ms();
    store.record_tree(repo, &commit, &paths, &blobs, EXTRACTOR_VERSION, indexed_at)?;
    Ok(IndexStats {
        rev: commit,
        files: paths.len(),
        read: blobs.len(),
        ms: started.elapsed().as_millis(),
    })
}

/// Files read for relations in one call, at most: a bound on a monorepo asked about as a whole.
pub const MAX_RELATION_FILES: usize = 5000;

/// The relations a task asked for at one revision ([`relations`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Relations {
    /// The commit (the full id) and its tree in the store.
    pub rev: String,
    pub tree: i64,
    /// What the task keeps, and what pruning dropped.
    pub pruned: Pruned,
    /// Files in scope, and blobs read this time (the others were read before, by this proposer version).
    pub files: usize,
    pub read: usize,
    /// More files were in scope than [`MAX_RELATION_FILES`]: narrow `scope`.
    pub truncated: bool,
}

/// Which files a task asked about: files and directories (`""` or `.` for all); everything when empty.
struct Scope {
    all: bool,
    names: std::collections::HashSet<String>,
}

impl Scope {
    fn new(scope: &[String]) -> Scope {
        let names: std::collections::HashSet<String> =
            scope.iter().map(|name| name.trim_matches('/').to_string()).collect();
        Scope {
            all: names.is_empty() || names.contains("") || names.contains("."),
            names,
        }
    }

    /// The path itself, or one of its directories, was named.
    fn holds(&self, path: &str) -> bool {
        self.all || self.names.contains(path) || path.match_indices('/').any(|(at, _)| self.names.contains(&path[..at]))
    }
}

/// The relations of `kinds` (all when empty) among the files under `scope` of `repo` at `rev` of the git
/// checkout `repo_dir`: the revision is indexed first (only blobs the store has not seen are read), the proposer
/// reads only blobs it has not read at [`RELATIONS_VERSION`], and the result is pruned for the task. Verdicts the
/// judge gave earlier come back with their relations.
pub fn relations(
    db: &Path,
    repo: &str,
    repo_dir: &Path,
    rev: &str,
    kinds: &[Kind],
    scope: &[String],
) -> Result<Relations, Error> {
    let indexed = index_tree(db, repo, repo_dir, rev)?;
    let all = blobs_at(repo_dir, &indexed.rev)?;
    let paths: std::collections::BTreeSet<String> = all.iter().map(|(path, _, _)| path.clone()).collect();
    let scope = Scope::new(scope);
    let mut files: Vec<(String, String)> = all
        .into_iter()
        .filter(|(path, _, size)| scanned(path) && *size <= MAX_BYTES && scope.holds(path))
        .map(|(path, oid, _)| (path, oid))
        .collect();
    let truncated = files.len() > MAX_RELATION_FILES;
    files.truncate(MAX_RELATION_FILES);
    let mut store = Store::open(db)?;
    let tree = store
        .tree_id(repo, &indexed.rev)?
        .ok_or_else(|| Error(format!("{repo} at {} was not recorded", indexed.rev)))?;
    let oids: Vec<&str> = files.iter().map(|(_, oid)| oid.as_str()).collect();
    let known = store.scanned_blobs(&oids, RELATIONS_VERSION)?;
    let mut unread: Vec<(String, String)> = Vec::new();
    let mut queued = std::collections::HashSet::new();
    for (path, oid) in &files {
        if !known.contains(oid) && queued.insert(oid.clone()) {
            unread.push((path.clone(), oid.clone()));
        }
    }
    let unread_oids: Vec<String> = unread.iter().map(|(_, oid)| oid.clone()).collect();
    let mut read = Vec::with_capacity(unread.len());
    for ((path, oid), body) in unread.iter().zip(read_blobs(repo_dir, &unread_oids)?) {
        // Text that is not UTF-8 is not source: recorded as read, with nothing in it.
        let proposed = String::from_utf8(body)
            .map(|text| crate::relations::propose(path, &text))
            .unwrap_or_default();
        read.push((oid.clone(), proposed));
    }
    store.record_relations(tree, &files, &read, RELATIONS_VERSION)?;
    let in_files: Vec<String> = files.iter().map(|(path, _)| path.clone()).collect();
    let rows = store.relations(tree, Some(&in_files))?;
    Ok(Relations {
        rev: indexed.rev,
        tree,
        pruned: prune(rows, kinds, &paths),
        files: files.len(),
        read: read.len(),
        truncated,
    })
}
