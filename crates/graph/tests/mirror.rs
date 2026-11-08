//! The Neo4j mirror and memorantado's memory relations: the statements sent, their parameters and credentials,
//! errors as text, and memorantado read page by page, without writing to it.

use crate::fakes::{FakeHttp, password, serve};
use config::keys::{Neo4jSettings, neo4j_settings};
use graph::MemorantadoSettings;
use graph::{Memorantado, Neo4j};
use kernel::Signal;
use knowledge::relations::{Kind, Relation};
use serde_json::json;
use std::sync::Arc;

fn settings(url: &str) -> Neo4jSettings {
    neo4j_settings(
        json!({"graph": {"neo4j": {"url": format!("{url}/")}}})
            .as_object()
            .unwrap(),
    )
    .unwrap()
}

fn relation(kind: Kind, subject: &str, object: &str, line: i64) -> Relation {
    Relation {
        id: line,
        kind,
        subject: subject.into(),
        object: object.into(),
        path: subject.split("::").next().unwrap().into(),
        blob: "b".repeat(40),
        line,
        quote: "q".into(),
        context: "c".into(),
        status: "accepted".into(),
        checked_by: Some("code".into()),
        rung: None,
        model: None,
        confidence: None,
    }
}

fn neo4j(http: &Arc<FakeHttp>, with_password: bool) -> Neo4j {
    let secret = with_password.then(password);
    Neo4j::new(
        http.clone(),
        settings("http://neo4j.test:7474"),
        Arc::new(move || secret.clone()),
    )
}

#[test]
fn graph_settings_read_neo4j_only_when_its_url_is_set() {
    let none = serde_json::Map::new();
    assert_eq!(neo4j_settings(&none), None);
    let set = settings("http://h:7474");
    assert_eq!(
        (set.url.as_str(), set.database.as_str(), set.user.as_str()),
        ("http://h:7474", "neo4j", "neo4j")
    );
    let named = neo4j_settings(
        json!({"graph": {"neo4j": {"url": " http://h ", "database": "code", "user": "ultron"}}})
            .as_object()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (named.url.as_str(), named.database.as_str(), named.user.as_str()),
        ("http://h", "code", "ultron")
    );
}

#[test]
fn graph_relations_are_merged_one_statement_per_kind_with_basic_auth() {
    let http = Arc::new(FakeHttp::default());
    let relations = [
        relation(Kind::ReadsConfig, "src/lib.rs::home", "STORE_HOME", 4),
        relation(Kind::TouchesTable, "src/db.rs::save", "events", 2),
        relation(Kind::ReadsConfig, "src/lib.rs::level", "log.level", 9),
    ];
    let mirrored = neo4j(&http, true)
        .upsert_relations("local:/r", "abc", &relations, &Signal::new())
        .unwrap();
    assert_eq!(mirrored, 3);
    let seen = http.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].url, "http://neo4j.test:7474/db/neo4j/query/v2");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Basic {}", jsrt::base64::encode(format!("neo4j:{}", password()))).as_str())
    );
    let statements = http.statements();
    assert!(
        statements[0]
            .0
            .contains("MERGE (o:ConfigKey {repo: $repo, name: row.object})")
    );
    assert!(
        statements[0]
            .0
            .contains("[r:READS_CONFIG {repo: $repo, path: row.path, line: row.line}]")
    );
    assert_eq!(statements[0].1["rows"].as_array().unwrap().len(), 2);
    assert_eq!(statements[0].1["rows"][0]["evidence"], "repo://src/lib.rs#L4");
    assert_eq!(
        (statements[0].1["repo"].clone(), statements[0].1["rev"].clone()),
        (json!("local:/r"), json!("abc"))
    );
    assert!(statements[1].0.contains("TOUCHES_TABLE") && statements[1].0.contains(":Table"));
    // No password: no authorization header (Neo4j without authentication).
    let open = Arc::new(FakeHttp::default());
    neo4j(&open, false)
        .upsert_relations("local:/r", "abc", &relations[..1], &Signal::new())
        .unwrap();
    assert_eq!(open.seen()[0].authorization, None);
}

#[test]
fn graph_neo4j_failures_are_text_without_the_password() {
    let http = Arc::new(FakeHttp::default());
    *http.neo4j_status.lock().unwrap() = Some(401);
    let error = neo4j(&http, true)
        .run("RETURN 1", json!({}), &Signal::new())
        .unwrap_err();
    assert_eq!(error, "Neo4j refused the credentials of neo4j");
    *http.neo4j_status.lock().unwrap() = None;
    *http.neo4j_body.lock().unwrap() =
        Some(json!({"errors": [{"code": "Neo.ClientError.Statement.SyntaxError", "message": "Invalid input"}]}));
    assert_eq!(
        neo4j(&http, true).run("RETURN", json!({}), &Signal::new()).unwrap_err(),
        "Neo4j: Invalid input"
    );
    *http.down.lock().unwrap() = true;
    let error = neo4j(&http, true)
        .run("RETURN 1", json!({}), &Signal::new())
        .unwrap_err();
    assert_eq!(error, "Neo4j at http://neo4j.test:7474 could not be reached");
    assert!(!error.contains(&password()));
    let mut bad = settings("http://neo4j.test");
    bad.database = "a/b".into();
    let error = Neo4j::new(http.clone(), bad, Arc::new(|| None))
        .run("RETURN 1", json!({}), &Signal::new())
        .unwrap_err();
    assert_eq!(error, "graph.neo4j.database \"a/b\" is not a database name");
}

#[test]
fn graph_replacing_a_repository_clears_it_first_and_drops_lone_nodes_after() {
    let http = Arc::new(FakeHttp::default());
    neo4j(&http, false)
        .replace_relations(
            "local:/r",
            "abc",
            &[relation(Kind::Tests, "t/it.rs", "src/db.rs", 1)],
            &Signal::new(),
        )
        .unwrap();
    let statements: Vec<String> = http.statements().into_iter().map(|(s, _)| s).collect();
    assert_eq!(statements.len(), 3);
    assert_eq!(
        statements[0],
        "MATCH ()-[r {repo: $repo, source: 'ultron'}]->() DELETE r"
    );
    assert!(statements[1].contains(":TESTS") && statements[1].contains("MERGE (o:Code"));
    assert!(statements[2].starts_with("MATCH (n {repo: $repo})") && statements[2].ends_with("DELETE n"));
}

#[test]
fn graph_memorantado_is_read_page_by_page_and_mirrored_with_its_links() {
    let http = Arc::new(FakeHttp::default());
    let mut memories: Vec<serde_json::Value> = (1..=501)
        .map(|id| {
            json!({"id": id, "status": "active", "kind": "fact", "subject": "s", "content": "x".repeat(400),
            "tags": ["project:shop"], "key": null})
        })
        .collect();
    memories.push(
        json!({"id": 600, "status": "superseded", "kind": "convention", "subject": "tests",
        "content": "old", "tags": [], "key": "k"}),
    );
    memories[0]["supersedesId"] = json!(600);
    *http.memories.lock().unwrap() = memories;
    *http.conflicts.lock().unwrap() = vec![
        json!({"id": 1, "memoryId": 2, "conflictingId": 3, "reason": "exclusive",
        "resolutionStatus": "open"}),
    ];
    let settings = MemorantadoSettings {
        url: "http://mem.test/api".into(),
        project: "my shop".into(),
    };
    let memorantado = Memorantado::new(http.clone(), settings);
    let graph = memorantado.read(&Signal::new()).unwrap();
    assert_eq!(graph.memories.len(), 502);
    assert_eq!(graph.memories[0].content.len(), 300);
    assert_eq!(graph.supersedes, [(1, 600)]);
    assert_eq!(graph.conflicts, [(2, 3, "exclusive".to_string())]);
    let urls: Vec<String> = http.seen().into_iter().map(|s| s.url).collect();
    assert_eq!(
        urls,
        [
            "http://mem.test/api/semantic-memories?project=my%20shop&status=active&limit=500&offset=0",
            "http://mem.test/api/semantic-memories?project=my%20shop&status=active&limit=500&offset=500",
            "http://mem.test/api/semantic-memories?project=my%20shop&status=superseded&limit=500&offset=0",
            "http://mem.test/api/semantic-memories?project=my%20shop&status=invalidated&limit=500&offset=0",
            "http://mem.test/api/memory-conflicts?project=my%20shop",
        ]
    );
    // Only read: every request to memorantado is a GET.
    assert!(http.seen().iter().all(|s| s.method == "GET"));
    let mirror = Arc::new(FakeHttp::default());
    let count = neo4j(&mirror, false)
        .replace_memories("my shop", &graph, &Signal::new())
        .unwrap();
    assert_eq!(count, 502);
    let statements = mirror.statements();
    assert_eq!(
        statements[0].0,
        "MATCH (m:Memory {source: 'memorantado', project: $project}) DETACH DELETE m"
    );
    // 502 memories in batches of 500, then one statement for each kind of link.
    assert_eq!(statements.len(), 5);
    assert_eq!(statements[1].1["rows"].as_array().unwrap().len(), 500);
    assert!(statements[3].0.contains("CREATE (a)-[:SUPERSEDES]->(b)"));
    assert_eq!(
        statements[4].1["rows"][0],
        json!({"from": 2, "to": 3, "reason": "exclusive"})
    );
}

#[test]
fn graph_neo4j_over_http_posts_the_query_api() {
    let (base, seen) = serve();
    let transport = Arc::new(blocking_http::ReqwestTransport::new().unwrap());
    let secret = password();
    let neo4j = Neo4j::new(transport, settings(&base), Arc::new(move || Some(secret.clone())));
    neo4j
        .upsert_relations(
            "local:/r",
            "abc",
            &[relation(Kind::ServesRoute, "src/server.rs::health", "GET /health", 5)],
            &Signal::new(),
        )
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "/db/neo4j/query/v2");
    assert!(seen[0].1.starts_with("Basic "));
    assert!(seen[0].2["statement"].as_str().unwrap().contains("SERVES_ROUTE"));
    assert_eq!(seen[0].2["parameters"]["rows"][0]["object"], "GET /health");
}
