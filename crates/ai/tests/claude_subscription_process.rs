//! Ports of the Hermes plugin's `test_directsdk_discovery.py` missing-CLI and resolution cases: locating `claude`;
//! plus the inert MCP server's replies.

mod claude_subscription_support;

use ai::claude_subscription::Env;
use ai::claude_subscription::inert_mcp::{DENIAL, reply, serve};
use ai::claude_subscription::process::resolve_claude;
use claude_subscription_support::*;
use serde_json::json;
use std::path::Path;

fn install(path: &Path) -> String {
    std::fs::create_dir_all(path).unwrap();
    let binary = path.join("claude");
    std::fs::write(&binary, "").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    binary.display().to_string()
}

fn service_env(home: &Path) -> Env {
    [
        ("HOME", home.display().to_string()),
        ("PATH", home.join("empty").display().to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

#[test]
fn ai_claude_subscription_resolves_claude_outside_a_service_path() {
    seal();
    for prefix in [
        ".local/bin",
        ".claude/local",
        "bin",
        ".npm-global/bin",
        ".bun/bin",
        ".volta/bin",
    ] {
        let home = tempfile::tempdir().unwrap();
        let binary = install(&home.path().join(prefix));
        let resolved = resolve_claude(Some(&["claude".into(), "--verbose".into()]), &service_env(home.path())).unwrap();
        assert_eq!(resolved, [binary, "--verbose".to_string()], "{prefix}");
    }
}

#[test]
fn ai_claude_subscription_path_and_explicit_commands_take_precedence() {
    seal();
    let home = tempfile::tempdir().unwrap();
    let probed = install(&home.path().join(".local/bin"));
    let on_path = install(&home.path().join("path-bin"));
    let mut env = service_env(home.path());
    env.insert("PATH".into(), home.path().join("path-bin").display().to_string());
    assert_eq!(resolve_claude(None, &env).unwrap(), std::slice::from_ref(&on_path));
    let mut overridden = env.clone();
    overridden.insert(ai::claude_subscription::process::COMMAND_ENV.into(), probed.clone());
    assert_eq!(resolve_claude(None, &overridden).unwrap(), [probed]);
    assert_eq!(
        resolve_claude(Some(&[home.path().join("missing/claude").display().to_string()]), &env),
        None
    );
    // Only the bare default name is probed outside PATH: an explicit wrapper is the user's choice.
    assert_eq!(resolve_claude(Some(&["custom-claude-wrapper".into()]), &env), None);
    // The env's PATH decides, never the test runner's.
    let mut nowhere = Env::new();
    nowhere.insert("PATH".into(), home.path().join("nowhere").display().to_string());
    assert_eq!(resolve_claude(None, &nowhere), None);
    let mut defaulted = Env::new();
    defaulted.insert("PATH".into(), "/bin:/usr/bin".into());
    assert_eq!(resolve_claude(None, &Env::new()), resolve_claude(None, &defaulted));
}

#[test]
fn ai_claude_subscription_inert_mcp_serves_the_inventory_and_denies_every_call() {
    seal();
    let manifest = json!([{"name": "rlm", "description": "d", "inputSchema": {"type": "object"}}]);
    assert_eq!(
        reply(
            &manifest,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#
        )
        .unwrap(),
        json!({"jsonrpc": "2.0", "id": 1, "result": {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "ultron-inert-inventory", "version": "1"}}})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#).unwrap()["result"],
        json!({"tools": manifest})
    );
    assert_eq!(
        reply(
            &manifest,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rlm","arguments":{"code":"import os"}}}"#
        )
        .unwrap()["result"],
        json!({"isError": true, "content": [{"type": "text", "text": DENIAL}]})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","id":4,"method":"ping"}"#).unwrap()["result"],
        json!({})
    );
    assert_eq!(
        reply(&manifest, r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        None
    );
    assert_eq!(reply(&manifest, "not json"), None);
    let mut out = Vec::new();
    serve(
        &manifest,
        "{\"id\":1,\"method\":\"tools/list\"}\n\n{\"method\":\"x\"}\n".as_bytes(),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
    // The standalone binary refuses a missing manifest argument.
    let status = std::process::Command::new(inert_mcp())
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!status.success());
}
