//! The store: schema, versioning and the writes the indexer makes.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::extract::{Call, Construct, Extracted, Import, Symbol};
use crate::relations::{Kind, Proposed, Relation};

/// The schema the kernel reader (`rlm/knowledge_api.py`) is written against. Bump it with every change to a table the
/// reader uses, and teach the reader the new version in the same change.
pub const SCHEMA_VERSION: i64 = 5;

/// Characters of a note's body kept.
pub const NOTE_CHARS: usize = 2000;

/// Trees kept per repository: enough for the reviews in flight and their re-reviews; older ones are dropped.
pub const TREES_PER_REPO: i64 = 50;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS repos (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
-- One row per indexed blob; `extractor` is the extractor version that read it.
CREATE TABLE IF NOT EXISTS blobs (oid TEXT PRIMARY KEY, lang TEXT NOT NULL, extractor INTEGER NOT NULL);
-- What a blob is made of: each construct kind once, at its first line, with how often it occurs.
CREATE TABLE IF NOT EXISTS constructs (
    blob TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, count INTEGER NOT NULL,
    PRIMARY KEY (blob, kind)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS constructs_kind ON constructs (kind, blob);
-- Schema 4 added `end_line` and `parent` (the symbol whose definition contains it).
CREATE TABLE IF NOT EXISTS symbols (
    blob TEXT NOT NULL, name TEXT NOT NULL, kind TEXT NOT NULL, line INTEGER NOT NULL, end_line INTEGER, parent TEXT
);
CREATE INDEX IF NOT EXISTS symbols_blob ON symbols (blob);
CREATE INDEX IF NOT EXISTS symbols_name ON symbols (name);
-- Schema 4. The graph a review queries (`knowledge_api.Knowledge.query`): call sites by the symbol they are made
-- from, and the names each import brings in, unresolved.
CREATE TABLE IF NOT EXISTS calls (
    blob TEXT NOT NULL, caller TEXT, callee TEXT NOT NULL, receiver TEXT, line INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS calls_blob ON calls (blob);
CREATE INDEX IF NOT EXISTS calls_callee ON calls (callee);
CREATE TABLE IF NOT EXISTS imports (blob TEXT NOT NULL, module TEXT NOT NULL, name TEXT, line INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS imports_blob ON imports (blob);
CREATE INDEX IF NOT EXISTS imports_module ON imports (module);
-- A repository at one commit: its indexable paths and their blobs.
CREATE TABLE IF NOT EXISTS trees (
    id INTEGER PRIMARY KEY, repo INTEGER NOT NULL, rev TEXT NOT NULL, indexed_at INTEGER NOT NULL,
    UNIQUE (repo, rev)
);
CREATE TABLE IF NOT EXISTS entries (
    tree INTEGER NOT NULL, path TEXT NOT NULL, blob TEXT NOT NULL,
    PRIMARY KEY (tree, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS entries_blob ON entries (blob);
-- Schema 2. Review comments left on the repository's pull requests, by anyone: what reviewers asked of a file,
-- and whether it was addressed (`addressed`: the thread was resolved; `outdated`: only a later commit changed its
-- lines; `open`).
-- Schema 3. A human's addressed comment is a rule the investigators can ask for (`rules_for`): `human` (not a bot,
-- not one of the reviewing accounts), `missed` (this reviewer reviewed the pull request and raised nothing there),
-- and the fates of the findings that cited it (`accepted`, `rejected`), its credit.
CREATE TABLE IF NOT EXISTS notes (
    id INTEGER PRIMARY KEY, repo INTEGER NOT NULL, pull INTEGER NOT NULL, path TEXT NOT NULL, line INTEGER,
    author TEXT NOT NULL, body TEXT NOT NULL, url TEXT NOT NULL, at TEXT NOT NULL, state TEXT NOT NULL,
    human INTEGER NOT NULL DEFAULT 1, missed INTEGER NOT NULL DEFAULT 0,
    accepted INTEGER NOT NULL DEFAULT 0, rejected INTEGER NOT NULL DEFAULT 0,
    UNIQUE (repo, url)
);
CREATE INDEX IF NOT EXISTS notes_path ON notes (repo, path);
-- Schema 5. Relations across files (`relations.rs`), read on demand for a task: what the proposer read from a blob
-- (`relation_scans`: by which proposer version, even when it found nothing), each relation with its evidence and
-- verdict (`status` accepted, rejected, review or proposed; `checked_by` code or the judge backend), and which
-- files of a revision were read for relations (`relation_entries`, beside the indexer's `entries`).
CREATE TABLE IF NOT EXISTS relation_scans (
    blob TEXT NOT NULL, version INTEGER NOT NULL, PRIMARY KEY (blob, version)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS relations (
    id INTEGER PRIMARY KEY, blob TEXT NOT NULL, kind TEXT NOT NULL, subject TEXT NOT NULL, object TEXT NOT NULL,
    line INTEGER NOT NULL, quote TEXT NOT NULL, context TEXT NOT NULL, status TEXT NOT NULL, checked_by TEXT,
    rung TEXT, model TEXT, confidence REAL
);
CREATE INDEX IF NOT EXISTS relations_blob ON relations (blob);
CREATE TABLE IF NOT EXISTS relation_entries (
    tree INTEGER NOT NULL, path TEXT NOT NULL, blob TEXT NOT NULL,
    PRIMARY KEY (tree, path)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS relation_entries_blob ON relation_entries (blob);
";

/// A rule (a person's addressed review comment) with the fates of the findings that cited it.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleCredit {
    pub id: i64,
    pub path: String,
    pub body: String,
    pub url: String,
    pub accepted: i64,
    pub rejected: i64,
}

/// A review comment, as [`Store::record_notes`] keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub pull: i64,
    pub path: String,
    pub line: Option<i64>,
    pub author: String,
    pub body: String,
    /// The comment's URL: a note is recorded once, its state updated.
    pub url: String,
    /// ISO time the comment was made.
    pub at: String,
    /// `addressed` (resolved), `outdated` (a later commit changed its lines, unresolved) or `open`.
    pub state: String,
    /// Written by a person: not a bot, not a reviewing account. Only a person's addressed comment is a rule.
    pub human: bool,
    /// This reviewer reviewed the pull request and raised nothing at the comment's place: a miss to learn from.
    pub missed: bool,
}

/// The judge's verdict on a relation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    Accepted,
    Rejected,
    /// The judge was not sure and nothing settled it: a person may.
    Review,
}

impl Settled {
    pub fn name(self) -> &'static str {
        match self {
            Settled::Accepted => "accepted",
            Settled::Rejected => "rejected",
            Settled::Review => "review",
        }
    }

    pub fn parse(name: &str) -> Option<Settled> {
        [Settled::Accepted, Settled::Rejected, Settled::Review]
            .into_iter()
            .find(|settled| settled.name() == name)
    }
}

/// [`Store::settle_relations`]: one relation's verdict and who gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Settlement {
    pub id: i64,
    pub status: Settled,
    pub backend: Option<String>,
    pub rung: Option<String>,
    pub model: Option<String>,
    pub confidence: Option<f64>,
}

/// The columns schema 4 added to `symbols`. A store of an earlier schema has its blobs read again anyway: they were
/// read by an older extractor.
const SYMBOL_COLUMNS_4: [(&str, &str); 2] = [("end_line", "INTEGER"), ("parent", "TEXT")];

/// The columns schema 3 added to `notes`, for a store of schema 2.
const NOTE_COLUMNS_3: [(&str, &str); 4] = [
    ("human", "INTEGER NOT NULL DEFAULT 1"),
    ("missed", "INTEGER NOT NULL DEFAULT 0"),
    ("accepted", "INTEGER NOT NULL DEFAULT 0"),
    ("rejected", "INTEGER NOT NULL DEFAULT 0"),
];

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        Self(format!("knowledge store: {error}"))
    }
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Opens (creating when absent) the store at `path`, private to the user. WAL lets the kernel read while a
    /// review indexes; writers wait for each other instead of failing.
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| Error(format!("{}: {error}", parent.display())))?;
        }
        let conn = Connection::open(path)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        conn.busy_timeout(Duration::from_secs(60))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), Error> {
        self.conn.execute_batch(SCHEMA)?;
        let found: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match found.as_deref().map(str::parse::<i64>) {
            None => {
                self.conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(Ok(SCHEMA_VERSION)) => {}
            // Schema 1 had no notes (the table was created above); schema 2's lack the rule columns; schema 3's
            // symbols lack their ranges and parents; schema 4's relations tables were created above.
            Some(Ok(old @ 1..=4)) => {
                let columns = |table: &str| -> Result<Vec<String>, Error> {
                    let mut statement = self
                        .conn
                        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
                    let names = statement.query_map([], |row| row.get(0))?.collect::<Result<_, _>>()?;
                    Ok(names)
                };
                let add = |table: &str, have: &[String], wanted: &[(&str, &str)]| -> Result<(), Error> {
                    for (column, definition) in wanted {
                        if !have.iter().any(|name| name == column) {
                            self.conn
                                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
                        }
                    }
                    Ok(())
                };
                add("notes", &columns("notes")?, &NOTE_COLUMNS_3)?;
                add("symbols", &columns("symbols")?, &SYMBOL_COLUMNS_4)?;
                if old <= 2 {
                    // Schema 2 counted an outdated thread as addressed: which of its addressed notes were resolved
                    // is unknown, so none is a rule until the backfill, run again, reads the recent ones' threads.
                    self.conn
                        .execute("UPDATE notes SET state = 'outdated' WHERE state = 'addressed'", [])?;
                    self.conn
                        .execute("DELETE FROM meta WHERE key LIKE 'notes_backfilled:%'", [])?;
                }
                self.conn.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(Ok(version)) if version > SCHEMA_VERSION => {
                return Err(Error(format!(
                    "the knowledge store has schema {version}, newer than this build's {SCHEMA_VERSION}"
                )));
            }
            Some(other) => {
                return Err(Error(format!(
                    "the knowledge store has an unknown schema version ({other:?})"
                )));
            }
        }
        Ok(())
    }

    /// The blobs among `oids` that are already indexed by extractor `version`.
    pub fn known_blobs(&self, oids: &[&str], version: i64) -> Result<std::collections::HashSet<String>, Error> {
        let mut known = std::collections::HashSet::new();
        let mut statement = self
            .conn
            .prepare_cached("SELECT 1 FROM blobs WHERE oid = ?1 AND extractor = ?2")?;
        for oid in oids {
            if statement.exists(params![oid, version])? {
                known.insert((*oid).to_string());
            }
        }
        Ok(known)
    }

    /// Records one revision: the newly read blobs, then the tree's path-to-blob table, in one transaction. Returns
    /// the tree's id.
    pub fn record_tree(
        &mut self,
        repo: &str,
        rev: &str,
        entries: &[(String, String)],
        blobs: &[(String, String, Extracted)],
        version: i64,
        indexed_at: i64,
    ) -> Result<i64, Error> {
        let tx = self.conn.transaction()?;
        {
            let mut blob_row = tx.prepare_cached(
                "INSERT INTO blobs (oid, lang, extractor) VALUES (?1, ?2, ?3)
                 ON CONFLICT (oid) DO UPDATE SET lang = excluded.lang, extractor = excluded.extractor",
            )?;
            let mut clear_constructs = tx.prepare_cached("DELETE FROM constructs WHERE blob = ?1")?;
            let mut clear_symbols = tx.prepare_cached("DELETE FROM symbols WHERE blob = ?1")?;
            let mut clear_calls = tx.prepare_cached("DELETE FROM calls WHERE blob = ?1")?;
            let mut clear_imports = tx.prepare_cached("DELETE FROM imports WHERE blob = ?1")?;
            let mut construct_row =
                tx.prepare_cached("INSERT INTO constructs (blob, kind, line, count) VALUES (?1, ?2, ?3, ?4)")?;
            let mut symbol_row = tx.prepare_cached(
                "INSERT INTO symbols (blob, name, kind, line, end_line, parent) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut call_row = tx.prepare_cached(
                "INSERT INTO calls (blob, caller, callee, receiver, line) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut import_row =
                tx.prepare_cached("INSERT INTO imports (blob, module, name, line) VALUES (?1, ?2, ?3, ?4)")?;
            for (oid, lang, extracted) in blobs {
                blob_row.execute(params![oid, lang, version])?;
                clear_constructs.execute(params![oid])?;
                clear_symbols.execute(params![oid])?;
                clear_calls.execute(params![oid])?;
                clear_imports.execute(params![oid])?;
                for Construct { kind, line, count } in &extracted.constructs {
                    construct_row.execute(params![oid, kind, line, count])?;
                }
                for Symbol {
                    name,
                    kind,
                    line,
                    end_line,
                    parent,
                } in &extracted.symbols
                {
                    symbol_row.execute(params![oid, name, kind, line, end_line, parent])?;
                }
                for Call {
                    caller,
                    callee,
                    receiver,
                    line,
                } in &extracted.calls
                {
                    call_row.execute(params![oid, caller, callee, receiver, line])?;
                }
                for Import { module, name, line } in &extracted.imports {
                    import_row.execute(params![oid, module, name, line])?;
                }
            }
        }
        tx.execute(
            "INSERT INTO repos (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![repo],
        )?;
        let repo_id: i64 = tx.query_row("SELECT id FROM repos WHERE name = ?1", params![repo], |row| row.get(0))?;
        tx.execute(
            "INSERT INTO trees (repo, rev, indexed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo, rev) DO UPDATE SET indexed_at = excluded.indexed_at",
            params![repo_id, rev, indexed_at],
        )?;
        let tree: i64 = tx.query_row(
            "SELECT id FROM trees WHERE repo = ?1 AND rev = ?2",
            params![repo_id, rev],
            |row| row.get(0),
        )?;
        tx.execute("DELETE FROM entries WHERE tree = ?1", params![tree])?;
        {
            let mut entry_row = tx.prepare_cached("INSERT INTO entries (tree, path, blob) VALUES (?1, ?2, ?3)")?;
            for (path, oid) in entries {
                entry_row.execute(params![tree, path, oid])?;
            }
        }
        // Older trees of this repository beyond the limit go, with the blobs no tree still holds. A tree read only
        // for relations (a repository with no file the indexer reads) has no entries but is kept like the others.
        for table in ["entries", "relation_entries"] {
            tx.execute(
                &format!(
                    "DELETE FROM {table} WHERE tree IN (
                         SELECT id FROM trees WHERE repo = ?1 ORDER BY indexed_at DESC, id DESC LIMIT -1 OFFSET ?2)"
                ),
                params![repo_id, TREES_PER_REPO],
            )?;
        }
        let dropped = tx.execute(
            "DELETE FROM trees WHERE repo = ?1 AND id != ?2
                 AND id NOT IN (SELECT tree FROM entries UNION SELECT tree FROM relation_entries)",
            params![repo_id, tree],
        )?;
        if dropped > 0 {
            tx.execute_batch(
                "DELETE FROM constructs WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM symbols WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM calls WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM imports WHERE blob NOT IN (SELECT blob FROM entries);
                 DELETE FROM blobs WHERE oid NOT IN (SELECT blob FROM entries);
                 DELETE FROM relations WHERE blob NOT IN (SELECT blob FROM relation_entries);
                 DELETE FROM relation_scans WHERE blob NOT IN (SELECT blob FROM relation_entries);",
            )?;
        }
        tx.commit()?;
        Ok(tree)
    }

    /// Records review comments of `repo`, in one transaction: a new one is added, a known one (by URL) gets its
    /// current state, line and body. Returns how many were new.
    pub fn record_notes(&mut self, repo: &str, notes: &[Note]) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO repos (name) VALUES (?1) ON CONFLICT (name) DO NOTHING",
            params![repo],
        )?;
        let repo_id: i64 = tx.query_row("SELECT id FROM repos WHERE name = ?1", params![repo], |row| row.get(0))?;
        let mut added = 0;
        {
            let mut known = tx.prepare_cached("SELECT 1 FROM notes WHERE repo = ?1 AND url = ?2")?;
            // A miss stays one: the pull request's findings do not change once it closed.
            let mut row = tx.prepare_cached(
                "INSERT INTO notes (repo, pull, path, line, author, body, url, at, state, human, missed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT (repo, url) DO UPDATE SET
                     path = excluded.path, line = excluded.line, body = excluded.body, state = excluded.state,
                     human = excluded.human, missed = MAX(missed, excluded.missed)",
            )?;
            for note in notes {
                if !known.exists(params![repo_id, note.url])? {
                    added += 1;
                }
                let body: String = note.body.chars().take(NOTE_CHARS).collect();
                row.execute(params![
                    repo_id,
                    note.pull,
                    note.path,
                    note.line,
                    note.author,
                    body,
                    note.url,
                    note.at,
                    note.state,
                    note.human,
                    note.missed
                ])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Credit to rules (notes, by id) from the fates of the findings that cited them: `true` accepted. Ids of
    /// another repository are passed over. Returns how many rules were credited.
    pub fn credit_rules(&mut self, repo: &str, credits: &[(i64, bool)]) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut credited = 0;
        {
            let mut row = tx.prepare_cached(
                "UPDATE notes SET accepted = accepted + ?3, rejected = rejected + ?4
                 WHERE id = ?1 AND repo = (SELECT id FROM repos WHERE name = ?2)",
            )?;
            for (id, accepted) in credits {
                let (yes, no) = if *accepted { (1, 0) } else { (0, 1) };
                credited += row.execute(params![id, repo, yes, no])?;
            }
        }
        tx.commit()?;
        Ok(credited)
    }

    /// Marks the comments of `logins` (the reviewing accounts) and of GitHub Apps (`...[bot]`) as not a person's:
    /// the reviewer learns from people, never from its own text. Returns how many notes changed.
    pub fn mark_not_human(&self, repo: &str, logins: &[String]) -> Result<usize, Error> {
        let mut changed = self.conn.execute(
            "UPDATE notes SET human = 0
             WHERE human = 1 AND author LIKE '%[bot]' AND repo = (SELECT id FROM repos WHERE name = ?1)",
            params![repo],
        )?;
        for login in logins {
            changed += self.conn.execute(
                "UPDATE notes SET human = 0
                 WHERE human = 1 AND lower(author) = lower(?2) AND repo = (SELECT id FROM repos WHERE name = ?1)",
                params![repo, login],
            )?;
        }
        Ok(changed)
    }

    /// The rules of `repo` findings have cited (credit from at least one fate), most accepted first.
    pub fn credited_rules(&self, repo: &str) -> Result<Vec<RuleCredit>, Error> {
        let mut rows = self.conn.prepare_cached(
            "SELECT n.id, n.path, n.body, n.url, n.accepted, n.rejected FROM notes n JOIN repos r ON r.id = n.repo
             WHERE r.name = ?1 AND n.accepted + n.rejected > 0 ORDER BY n.accepted DESC, n.id",
        )?;
        let rules = rows
            .query_map(params![repo], |row| {
                Ok(RuleCredit {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    body: row.get(2)?,
                    url: row.get(3)?,
                    accepted: row.get(4)?,
                    rejected: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rules)
    }

    /// A rule's credit: (accepted, rejected, missed, human).
    pub fn rule_credit(&self, id: i64) -> Result<Option<(i64, i64, bool, bool)>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT accepted, rejected, missed, human FROM notes WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?)
    }

    /// The id of the note at `url` in `repo`.
    pub fn note_id(&self, repo: &str, url: &str) -> Result<Option<i64>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT n.id FROM notes n JOIN repos r ON r.id = n.repo WHERE r.name = ?1 AND n.url = ?2",
                params![repo, url],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// A value the host keeps beside the graph (`meta`), such as when a repository's notes were backfilled.
    pub fn meta(&self, key: &str) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), Error> {
        if key == "schema_version" {
            return Err(Error("the schema version is the store's own".into()));
        }
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The tree of `repo` at commit `rev`, when it was recorded.
    pub fn tree_id(&self, repo: &str, rev: &str) -> Result<Option<i64>, Error> {
        Ok(self
            .conn
            .query_row(
                "SELECT t.id FROM trees t JOIN repos r ON r.id = t.repo WHERE r.name = ?1 AND t.rev = ?2",
                params![repo, rev],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// The blobs among `oids` the relation proposer of `version` has read.
    pub fn scanned_blobs(&self, oids: &[&str], version: i64) -> Result<std::collections::HashSet<String>, Error> {
        let mut known = std::collections::HashSet::new();
        let mut statement = self
            .conn
            .prepare_cached("SELECT 1 FROM relation_scans WHERE blob = ?1 AND version = ?2")?;
        for oid in oids {
            if statement.exists(params![oid, version])? {
                known.insert((*oid).to_string());
            }
        }
        Ok(known)
    }

    /// Records what the proposer of `version` read from newly scanned blobs (their earlier proposals and verdicts
    /// go: they came from another version) and which files of `tree` were read for relations, in one transaction.
    /// A proposal plain code settled is `accepted` by `code`; the others wait for the judge.
    pub fn record_relations(
        &mut self,
        tree: i64,
        entries: &[(String, String)],
        scanned: &[(String, Vec<Proposed>)],
        version: i64,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        {
            let mut clear = tx.prepare_cached("DELETE FROM relations WHERE blob = ?1")?;
            let mut clear_scans = tx.prepare_cached("DELETE FROM relation_scans WHERE blob = ?1")?;
            let mut scan = tx.prepare_cached("INSERT INTO relation_scans (blob, version) VALUES (?1, ?2)")?;
            let mut row = tx.prepare_cached(
                "INSERT INTO relations (blob, kind, subject, object, line, quote, context, status, checked_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for (oid, proposed) in scanned {
                clear.execute(params![oid])?;
                clear_scans.execute(params![oid])?;
                scan.execute(params![oid, version])?;
                for relation in proposed {
                    let (status, checked_by) = if relation.certain {
                        ("accepted", Some("code"))
                    } else {
                        ("proposed", None)
                    };
                    row.execute(params![
                        oid,
                        relation.kind.name(),
                        relation.subject,
                        relation.object,
                        relation.line,
                        relation.quote,
                        relation.context,
                        status,
                        checked_by
                    ])?;
                }
            }
            let mut entry = tx.prepare_cached(
                "INSERT INTO relation_entries (tree, path, blob) VALUES (?1, ?2, ?3)
                 ON CONFLICT (tree, path) DO UPDATE SET blob = excluded.blob",
            )?;
            for (path, oid) in entries {
                entry.execute(params![tree, path, oid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The files of `tree` read for relations so far.
    pub fn relation_paths(&self, tree: i64) -> Result<Vec<String>, Error> {
        let mut statement = self
            .conn
            .prepare_cached("SELECT path FROM relation_entries WHERE tree = ?1 ORDER BY path")?;
        let paths = statement
            .query_map(params![tree], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(paths)
    }

    /// The relations of the files of `tree` read for relations, in path and line order; `paths` (when given)
    /// narrows them to those files.
    pub fn relations(&self, tree: i64, paths: Option<&[String]>) -> Result<Vec<Relation>, Error> {
        let mut statement = self.conn.prepare_cached(
            "SELECT r.id, r.kind, r.subject, r.object, e.path, r.blob, r.line, r.quote, r.context, r.status,
                    r.checked_by, r.rung, r.model, r.confidence
             FROM relation_entries e JOIN relations r ON r.blob = e.blob
             WHERE e.tree = ?1 ORDER BY e.path, r.line, r.id",
        )?;
        let wanted: Option<std::collections::HashSet<&str>> =
            paths.map(|paths| paths.iter().map(String::as_str).collect());
        let rows = statement.query_map(params![tree], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<f64>>(13)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                subject,
                object,
                path,
                blob,
                line,
                quote,
                context,
                status,
                checked_by,
                rung,
                model,
                confidence,
            ) = row?;
            if wanted.as_ref().is_some_and(|wanted| !wanted.contains(path.as_str())) {
                continue;
            }
            // A kind this build does not know came from another version; it is read again before it is used.
            let Some(kind) = Kind::parse(&kind) else { continue };
            let subject = if subject.is_empty() {
                path.clone()
            } else {
                format!("{path}::{subject}")
            };
            out.push(Relation {
                id,
                kind,
                subject,
                object,
                path,
                blob,
                line,
                quote,
                context,
                status,
                checked_by,
                rung,
                model,
                confidence,
            });
        }
        Ok(out)
    }

    /// Records the judge's verdicts on proposed relations (or ones it sent for review before), in one
    /// transaction; a relation plain code settled, or the judge already accepted or rejected, keeps its verdict.
    /// Returns how many changed.
    pub fn settle_relations(&mut self, verdicts: &[Settlement]) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let mut changed = 0;
        {
            let mut row = tx.prepare_cached(
                "UPDATE relations SET status = ?2, checked_by = ?3, rung = ?4, model = ?5, confidence = ?6
                 WHERE id = ?1 AND status IN ('proposed', 'review')",
            )?;
            for verdict in verdicts {
                changed += row.execute(params![
                    verdict.id,
                    verdict.status.name(),
                    verdict.backend,
                    verdict.rung,
                    verdict.model,
                    verdict.confidence
                ])?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// How many trees the store holds for `repo`.
    pub fn tree_count(&self, repo: &str) -> Result<i64, Error> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM trees t JOIN repos r ON r.id = t.repo WHERE r.name = ?1",
            params![repo],
            |row| row.get(0),
        )?)
    }
}
