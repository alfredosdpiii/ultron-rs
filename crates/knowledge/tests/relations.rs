//! Relations across files, read on demand (`knowledge::relations`): what plain code proposes and settles, what it
//! prunes, what waits for the judge, and the cache by blob that makes asking again free. The repository mixes a
//! Rust crate, a Python app, a TypeScript server and SQL migrations, as the repositories Ultron works in do.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};

use knowledge::relations::{Kind, RELATIONS_VERSION, Relation, propose, resolve_import};
use knowledge::{SCHEMA_VERSION, Settled, Settlement, Store, index_tree, relations};

const REPO: &str = "local:/work/shop";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Knowledge Test",
            "-c",
            "user.email=knowledge@test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const LIB: &str = r#"pub mod db;
pub mod server;

pub fn home() -> String {
    std::env::var("STORE_HOME").unwrap_or_default()
}

pub fn level(settings: &Settings) -> String {
    settings.get("log.level")
}
"#;

const DB: &str = r#"pub fn save(conn: &Connection) {
    conn.execute("INSERT INTO events (id) VALUES (?1)", []);
    let hint = "rows from inbox are archived";
}
"#;

const SERVER: &str = r#"pub fn router() -> Router {
    Router::new().route("/events", get(list_events))
}

#[get("/health")]
async fn health() -> &'static str {
    "ok"
}
"#;

const RUST_TEST: &str = r#"use std::path::Path;
use store::db;

#[test]
fn saves() {
    db::save(&conn());
}
"#;

const ROUTES: &str = r#"import os

@router.get("/users")
async def list_users():
    return os.environ["DB_URL"]
"#;

const PY_TEST: &str = r#"import pytest
from app.routes import list_users

def test_list():
    assert list_users()
"#;

const SERVER_TS: &str = r#"app.post('/login', login);
const secret = process.env.SECRET;
cache.get('/warm', refill);
"#;

/// One commit of the repository.
fn repository(root: &Path) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    write(root, "crates/store/Cargo.toml", "[package]\nname = \"store\"\n");
    write(root, "crates/store/src/lib.rs", LIB);
    write(root, "crates/store/src/db.rs", DB);
    write(root, "crates/store/src/server.rs", SERVER);
    write(root, "crates/store/tests/it.rs", RUST_TEST);
    write(
        root,
        "migrations/001.sql",
        "CREATE TABLE events (id INTEGER);\n-- from the old schema\n",
    );
    write(root, "app/routes.py", ROUTES);
    write(root, "app/test_routes.py", PY_TEST);
    write(root, "web/server.ts", SERVER_TS);
    write(root, "README.md", "Reads FROM inbox.\n");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    git(root, &["rev-parse", "HEAD"])
}

fn summary(relations: &[Relation]) -> Vec<String> {
    relations
        .iter()
        .map(|r| format!("{} {} -> {} [{}]", r.kind.name(), r.subject, r.object, r.status))
        .collect()
}

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    repository(&root);
    let db = dir.path().join("graph.db");
    (dir, root, db)
}

#[test]
fn plain_code_settles_what_nothing_else_could_mean_and_proposes_the_rest() {
    let (_dir, root, db) = setup();
    let found = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    assert_eq!(found.files, 8);
    assert_eq!(found.read, 8);
    assert!(!found.truncated);
    assert_eq!(
        summary(&found.pruned.kept),
        [
            "serves_route app/routes.py::list_users -> GET /users [accepted]",
            "reads_config app/routes.py::list_users -> DB_URL [accepted]",
            "tests app/test_routes.py -> app/routes.py [accepted]",
            "touches_table crates/store/src/db.rs::save -> events [accepted]",
            "touches_table crates/store/src/db.rs::save -> inbox [proposed]",
            "reads_config crates/store/src/lib.rs::home -> STORE_HOME [accepted]",
            "reads_config crates/store/src/lib.rs::level -> log.level [proposed]",
            "serves_route crates/store/src/server.rs::list_events -> GET /events [accepted]",
            "serves_route crates/store/src/server.rs::health -> GET /health [accepted]",
            "tests crates/store/tests/it.rs -> crates/store/src/db.rs [accepted]",
            "touches_table migrations/001.sql -> events [accepted]",
            "serves_route web/server.ts::login -> POST /login [accepted]",
            "reads_config web/server.ts -> SECRET [accepted]",
            "serves_route web/server.ts::refill -> GET /warm [proposed]",
        ]
    );
    // `use std::path::Path` and `import pytest` name no file of the repository.
    assert_eq!(found.pruned.dropped.get("unresolved"), Some(&2));
    let events = &found.pruned.kept[3];
    assert_eq!(
        (events.evidence(), events.quote.as_str(), events.checked_by.as_deref()),
        (
            "repo://crates/store/src/db.rs#L2".to_string(),
            r#"conn.execute("INSERT INTO events (id) VALUES (?1)", []);"#,
            Some("code")
        )
    );
    assert!(found.pruned.kept[4].context.contains("rows from inbox"));
}

#[test]
fn a_second_task_reads_nothing_again_and_keeps_the_judges_verdicts() {
    let (_dir, root, db) = setup();
    let first = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    let id = |object: &str| first.pruned.kept.iter().find(|r| r.object == object).unwrap().id;
    let mut store = Store::open(&db).unwrap();
    let verdict = |id, status| Settlement {
        id,
        status,
        backend: Some("d1".into()),
        rung: Some("first".into()),
        model: Some("d1-2b".into()),
        confidence: Some(0.93),
    };
    let settled = store
        .settle_relations(&[
            verdict(id("log.level"), Settled::Accepted),
            verdict(id("inbox"), Settled::Rejected),
            verdict(id("GET /warm"), Settled::Review),
            // Plain code settled this one; the judge cannot overturn it.
            verdict(id("events"), Settled::Rejected),
        ])
        .unwrap();
    assert_eq!(settled, 3);
    let again = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    assert_eq!(again.read, 0);
    assert_eq!(again.pruned.dropped.get("rejected"), Some(&1));
    let level = again.pruned.kept.iter().find(|r| r.object == "log.level").unwrap();
    assert_eq!(
        (level.status.as_str(), level.checked_by.as_deref(), level.confidence),
        ("accepted", Some("d1"), Some(0.93))
    );
    let warm = again.pruned.kept.iter().find(|r| r.object == "GET /warm").unwrap();
    assert_eq!(warm.status, "review");
    assert!(!again.pruned.kept.iter().any(|r| r.object == "inbox"));
    // A verdict the judge gave is final: a later one does not move it; a review can still be settled.
    let later = store
        .settle_relations(&[
            verdict(id("log.level"), Settled::Rejected),
            verdict(id("GET /warm"), Settled::Accepted),
        ])
        .unwrap();
    assert_eq!(later, 1);
}

/// Two tasks asked at once: both found the blob unread, the first recorded and judged it, then the second records
/// what it read. That keeps the first's rows: before, it replaced them, dropping the verdict (and its row id could
/// go to another relation, which a verdict in flight would then land on).
#[test]
fn a_scan_recorded_meanwhile_keeps_its_rows_and_verdicts() {
    let (_dir, root, db) = setup();
    let first = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    let level = first
        .pruned
        .kept
        .iter()
        .find(|r| r.object == "log.level")
        .unwrap()
        .clone();
    let mut store = Store::open(&db).unwrap();
    let settled = store
        .settle_relations(&[Settlement {
            id: level.id,
            status: Settled::Accepted,
            backend: Some("d1".into()),
            rung: None,
            model: None,
            confidence: None,
        }])
        .unwrap();
    assert_eq!(settled, 1);
    // The late scanner's read of the same blob, recorded after the first one's.
    let text = std::fs::read_to_string(root.join("crates/store/src/lib.rs")).unwrap();
    let late = vec![(level.blob.clone(), propose("crates/store/src/lib.rs", &text))];
    store
        .record_relations(first.tree, &[], &late, RELATIONS_VERSION)
        .unwrap();
    let again = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    let kept = again.pruned.kept.iter().find(|r| r.object == "log.level").unwrap();
    assert_eq!((kept.id, kept.status.as_str()), (level.id, "accepted"));
}

#[test]
fn a_task_asks_for_kinds_and_files() {
    let (_dir, root, db) = setup();
    let tests = relations(&db, REPO, &root, "HEAD", &[Kind::Tests], &[]).unwrap();
    assert_eq!(
        summary(&tests.pruned.kept),
        [
            "tests app/test_routes.py -> app/routes.py [accepted]",
            "tests crates/store/tests/it.rs -> crates/store/src/db.rs [accepted]",
        ]
    );
    assert_eq!(tests.pruned.dropped.get("kind"), Some(&12));
    let app = relations(&db, REPO, &root, "HEAD", &[], &["app/".into()]).unwrap();
    assert_eq!(app.files, 2);
    assert!(app.pruned.kept.iter().all(|r| r.path.starts_with("app/")));
    let one = relations(
        &db,
        REPO,
        &root,
        "HEAD",
        &[],
        &["web/server.ts".into(), "nothing".into()],
    )
    .unwrap();
    assert_eq!((one.files, one.read), (1, 0));
}

#[test]
fn a_new_commit_reads_only_its_changed_blob() {
    let (_dir, root, db) = setup();
    let first = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    let level = first.pruned.kept.iter().find(|r| r.object == "log.level").unwrap().id;
    Store::open(&db)
        .unwrap()
        .settle_relations(&[Settlement {
            id: level,
            status: Settled::Accepted,
            backend: Some("jev".into()),
            rung: Some("first".into()),
            model: None,
            confidence: Some(0.9),
        }])
        .unwrap();
    write(&root, "crates/store/src/db.rs", &DB.replace("events", "audit_events"));
    git(&root, &["commit", "-qam", "rename the table"]);
    let second = relations(&db, REPO, &root, "HEAD", &[Kind::TouchesTable, Kind::ReadsConfig], &[]).unwrap();
    assert_eq!(second.read, 1);
    assert_ne!(second.rev, first.rev);
    let objects: BTreeSet<&str> = second.pruned.kept.iter().map(|r| r.object.as_str()).collect();
    assert!(objects.contains("audit_events") && objects.contains("events"));
    let kept_level = second.pruned.kept.iter().find(|r| r.object == "log.level").unwrap();
    assert_eq!(kept_level.checked_by.as_deref(), Some("jev"));
    assert_eq!(Store::open(&db).unwrap().tree_count(REPO).unwrap(), 2);
}

#[test]
fn a_repository_with_no_indexed_language_keeps_its_relation_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "src/main.rs", "fn main() { let _ = std::env::var(\"PORT\"); }\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "one"]);
    let db = dir.path().join("graph.db");
    relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    write(&root, "src/main.rs", "fn main() { let _ = std::env::var(\"ADDR\"); }\n");
    git(&root, &["commit", "-qam", "two"]);
    let second = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    assert_eq!(
        summary(&second.pruned.kept),
        ["reads_config src/main.rs::main -> ADDR [accepted]"]
    );
    // Indexing a third revision does not take the first tree for an empty one.
    write(&root, "src/lib.rs", "pub fn x() {}\n");
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "three"]);
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    assert_eq!(Store::open(&db).unwrap().tree_count(REPO).unwrap(), 3);
}

#[test]
fn a_store_of_schema_4_gains_the_relation_tables() {
    let (_dir, root, db) = setup();
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "DROP TABLE relations; DROP TABLE relation_scans; DROP TABLE relation_entries;
             UPDATE meta SET value = '4' WHERE key = 'schema_version';",
        )
        .unwrap();
    }
    let store = Store::open(&db).unwrap();
    assert_eq!(store.tree_count(REPO).unwrap(), 1);
    drop(store);
    let found = relations(&db, REPO, &root, "HEAD", &[Kind::ServesRoute], &[]).unwrap();
    assert_eq!(found.pruned.kept.len(), 5);
    let version: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION.to_string());
    assert_eq!(SCHEMA_VERSION, 5);
}

#[test]
fn a_newer_proposer_reads_every_blob_again() {
    let (_dir, root, db) = setup();
    relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("UPDATE relation_scans SET version = ?1", [RELATIONS_VERSION - 1])
        .unwrap();
    let again = relations(&db, REPO, &root, "HEAD", &[], &[]).unwrap();
    assert_eq!(again.read, 8);
    assert_eq!(again.pruned.kept.len(), 14);
}

fn found(path: &str, text: &str) -> Vec<String> {
    propose(path, text)
        .into_iter()
        .map(|p| {
            format!(
                "{}:{} {} -> {}{}",
                p.line,
                p.kind.name(),
                p.subject,
                p.object,
                if p.certain { "" } else { " ?" }
            )
        })
        .collect()
}

#[test]
fn the_proposer_reads_code_not_prose() {
    // An import is no table; a comment is not read; prose in a string is only proposed.
    assert_eq!(
        found(
            "a.py",
            "from os import path\n# os.environ['IN_COMMENT']\nimport json\nnote = 'pick rows from inbox'\n"
        ),
        ["4:touches_table  -> inbox ?"]
    );
    // SQL over several lines of a string: the continuation line is SQL by itself.
    assert_eq!(
        found(
            "q.rs",
            "fn load() {\n    let q = \"SELECT id\n        FROM orders o\n        JOIN users u ON u.id = o.user_id\";\n}\n"
        ),
        ["3:touches_table load -> orders", "4:touches_table load -> users"]
    );
    // A TypeScript method is the subject; a Nest route is served by the method under it.
    assert_eq!(
        found(
            "c.ts",
            "export class Users {\n  @Get('me')\n  me() {\n    return process.env['TZ'];\n  }\n}\n"
        ),
        ["2:serves_route me -> GET /me", "4:reads_config me -> TZ"]
    );
    assert_eq!(
        found(
            "f.py",
            "@app.route('/x', methods=['POST'])\ndef handle():\n    return os.getenv('MODE')\n"
        ),
        ["1:serves_route handle -> POST /x", "3:reads_config handle -> MODE"]
    );
    assert_eq!(
        found(
            "schema.sql",
            "-- FROM nothing\nALTER TABLE IF EXISTS orders ADD x INT;\n"
        ),
        ["2:touches_table  -> orders"]
    );
    assert_eq!(
        found("m.py", "class Order(Base):\n    __tablename__ = 'orders'\n"),
        ["2:touches_table Order -> orders"]
    );
    // Not a test file: its imports are not test relations.
    assert!(found("x.py", "from app.routes import list_users\n").is_empty());
}

#[test]
fn test_imports_resolve_to_files_of_the_tree() {
    let paths: BTreeSet<String> = [
        "crates/judge-seat/src/lib.rs",
        "crates/judge-seat/src/seat/mod.rs",
        "crates/judge-seat/tests/fakes.rs",
        "pkg/app/__init__.py",
        "pkg/app/routes.py",
        "web/src/api/index.ts",
        "web/src/api/users.ts",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let resolved = |path: &str, module: &str| resolve_import(path, module, &paths);
    let test = "crates/judge-seat/tests/seat.rs";
    assert_eq!(
        resolved(test, "judge_seat::seat::Seat").as_deref(),
        Some("crates/judge-seat/src/seat/mod.rs")
    );
    assert_eq!(
        resolved(test, "judge_seat::Verdict").as_deref(),
        Some("crates/judge-seat/src/lib.rs")
    );
    assert_eq!(
        resolved(test, "crate::fakes::FakeTransport").as_deref(),
        Some("crates/judge-seat/tests/fakes.rs")
    );
    assert_eq!(resolved(test, "serde_json::json"), None);
    assert_eq!(
        resolved("pkg/tests/test_routes.py", "app.routes.list").as_deref(),
        Some("pkg/app/routes.py")
    );
    assert_eq!(
        resolved("pkg/app/test_x.py", ".routes").as_deref(),
        Some("pkg/app/routes.py")
    );
    assert_eq!(
        resolved("pkg/app/test_x.py", "..app").as_deref(),
        Some("pkg/app/__init__.py")
    );
    assert_eq!(
        resolved("web/src/api/users.test.ts", "./users").as_deref(),
        Some("web/src/api/users.ts")
    );
    assert_eq!(
        resolved("web/src/x.test.ts", "./api").as_deref(),
        Some("web/src/api/index.ts")
    );
    assert_eq!(resolved("web/src/x.test.ts", "react"), None);
}
