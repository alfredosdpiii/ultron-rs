//! `scan --online` lookups (loki.py `registry_url`, `dependency_violation`) with values recorded from loki.py.

use loki::registry::{dependency_violation, fetch, registry_url};

#[test]
fn loki_registry_urls_match_loki_py() {
    for (ecosystem, name, url) in [
        ("npm", "@scope/pkg", "https://registry.npmjs.org/%40scope%2Fpkg"),
        ("pypi", "requests", "https://pypi.org/pypi/requests/json"),
        ("crates", "serde_json", "https://crates.io/api/v1/crates/serde_json"),
        (
            "go",
            "github.com/Azure/Go-SDK",
            "https://proxy.golang.org/github.com/%21azure/%21go-%21s%21d%21k/@latest",
        ),
        ("hex", "phoenix_live", "https://hex.pm/api/packages/phoenix_live"),
        ("npm", "ünï code", "https://registry.npmjs.org/%C3%BCn%C3%AF%20code"),
        ("pypi", "a~b.c-d_e", "https://pypi.org/pypi/a~b.c-d_e/json"),
    ] {
        assert_eq!(registry_url(ecosystem, name), url, "{ecosystem} {name}");
    }
}

/// The answered lookups (found, 404, 503, a redirect) run end to end in hook_differential's `online-` cases.
#[test]
fn loki_registry_refused_connection_reads_as_loki_py_reports_it() {
    // A registry that refuses the connection (a port nothing listens on, on loopback).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let lookup = fetch(&format!("http://127.0.0.1:{port}/x"));
    assert_eq!(
        dependency_violation("npm", "pkg", lookup),
        Some("npm: registry check unavailable: pkg: <urlopen error [Errno 111] Connection refused>".into())
    );
}
