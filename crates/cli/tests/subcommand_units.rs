//! Literal expectations for the JavaScript semantics and helpers the ported subcommands print with (values checked
//! against Node: `String.prototype.replace`, `JSON.stringify(v, null, "\t")`).

use cli::js::replace_first;
use serde_json::json;

#[test]
fn cli_js_replace_first_expands_dollar_patterns() {
    // "a{{X}}b".replace("{{X}}", "[$&|$`|$'|$$|$1]") === "a[{{X}}|a|b|$|$1]b"
    assert_eq!(
        replace_first("a{{X}}b{{X}}", "{{X}}", "[$&|$`|$'|$$|$1]"),
        "a[{{X}}|a|b{{X}}|$|$1]b{{X}}"
    );
    assert_eq!(replace_first("abc", "z", "$&"), "abc");
}

#[test]
fn cli_mcp_edit_keeps_indentation_and_js_key_order() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nested/mcp.json");
    let config = |c: &str| json!({ "command": c }).as_object().unwrap().clone();
    assert!(!mcp::edit::add_server_config(&path, "a", config("x")).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n  \"mcpServers\": {\n    \"a\": {\n      \"command\": \"x\"\n    }\n  }\n}\n"
    );
    std::fs::write(&path, "{\n\t\"1\": 1.5e-7, \"mcpServers\": {\"a\": {}}, \"0\": []}").unwrap();
    assert!(mcp::edit::add_server_config(&path, "a", config("y")).unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{\n\t\"0\": [],\n\t\"1\": 1.5e-7,\n\t\"mcpServers\": {\n\t\t\"a\": {\n\t\t\t\"command\": \"y\"\n\t\t}\n\t}\n}\n"
    );
    assert!(mcp::edit::remove_server_config(&path, "a").unwrap());
    assert!(!mcp::edit::remove_server_config(&path, "a").unwrap());
    assert!(!mcp::edit::remove_server_config(&tmp.path().join("none.json"), "a").unwrap());
    std::fs::write(&path, "[]").unwrap();
    assert!(mcp::edit::add_server_config(&path, "a", config("y")).is_err());
}

#[test]
fn cli_usage_format_tokens_and_elapsed_match_report_text() {
    use usage::text::{format_elapsed, format_tokens};
    let tokens: Vec<String> = [999.0, 999_949.0, 999_950.0, 99_949_999.0, 99_950_000.0, 1234.0]
        .into_iter()
        .map(format_tokens)
        .collect();
    assert_eq!(tokens, ["999", "999.9k", "1.00M", "99.95M", "100.0M", "1.2k"]);
    let elapsed: Vec<String> = [499.0, 500.0, 59_499.0, 59_500.0, 3_599_499.0, 3_600_000.0, 7_322_000.0]
        .into_iter()
        .map(format_elapsed)
        .collect();
    assert_eq!(elapsed, ["0s", "1s", "59s", "1m00s", "59m59s", "1h00m", "2h02m"]);
}

#[test]
fn cli_export_derived_colors_follow_the_template_formula() {
    use cli::export_html::derive_export_colors;
    // deriveExportColors("#343541") (dark).
    assert_eq!(
        derive_export_colors("#343541"),
        ["rgb(36, 37, 46)", "rgb(44, 45, 55)", "rgb(72, 68, 65)"]
    );
    // A light base keeps itself as the card color.
    assert_eq!(
        derive_export_colors("#ffffff"),
        ["rgb(245, 245, 245)", "#ffffff", "rgb(255, 255, 235)"]
    );
    assert_eq!(
        derive_export_colors("rgb( 10 , 20 , 30 )"),
        ["rgb(7, 14, 21)", "rgb(9, 17, 26)", "rgb(30, 35, 30)"]
    );
    assert_eq!(
        derive_export_colors("nope"),
        ["rgb(24, 24, 30)", "rgb(30, 30, 36)", "rgb(60, 55, 40)"]
    );
}

#[test]
fn cli_usage_log_usage_matches_session_log() {
    use usage::log::log_usage;
    // logUsage({ input: 3, output: 4, cacheRead: -2 }): totals fall back to the sum of the positive parts.
    let usage = log_usage(Some(&json!({"input": 3, "output": 4, "cacheRead": -2}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cache_read, usage.cost), (7.0, 0.0, None));
    let usage = log_usage(Some(&json!({"input": 1, "totalTokens": 10, "cost": {"total": 0.5}}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cost), (10.0, Some(0.5)));
    let usage = log_usage(Some(&json!({"input": 2, "totalTokens": 0, "cost": {"total": -1}}))).unwrap();
    assert_eq!((usage.total_tokens, usage.cost), (2.0, None));
    assert!(log_usage(Some(&json!([1]))).is_none());
    assert!(log_usage(None).is_none());
}

/// `--last` is `Number(value)` then `Number.isSafeInteger` (usage-cli.ts); node 26.12.1: Number("0x10") === 16,
/// Number("0o17") === 15, Number("0b1") === 1, Number(" 7 ") === 7, and Number("1_0") and Number("-0x10") are NaN.
#[test]
fn cli_usage_last_reads_its_count_like_number() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let env = session_core::RunEnv {
        cwd: dir.path().to_path_buf(),
        home: dir.path().to_path_buf(),
        var: std::sync::Arc::new(|_| None),
        package: dir.path().to_path_buf(),
    };
    let refused = |count: &str| {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let args = [
            "usage",
            "--json",
            "--sessions-root",
            &root.display().to_string(),
            "--last",
            count,
        ]
        .map(str::to_string);
        let code = cli::usage_cli::run(&args, &env, &mut out, &mut err);
        let err = String::from_utf8_lossy(&err).into_owned();
        assert_eq!(
            code == Some(2),
            err.contains("--last needs a number between 1 and 1000"),
            "{count}: {err}"
        );
        code == Some(2)
    };
    for count in ["0x10", "0o17", "0b1", " 7 ", "1e3"] {
        assert!(!refused(count), "{count} is a count");
    }
    for count in ["1_0", "-0x10", "0x0", "1001", "Infinity"] {
        assert!(refused(count), "{count} is refused");
    }
}

/// `mcp add --oauth-callback-port` stores `Number(port)` (extensions/mcp/cli.ts); node 26.12.1: Number("0x1F90")
/// === 8080 and Number(" 8081 ") === 8081.
#[test]
fn cli_mcp_add_reads_the_callback_port_like_number() {
    for (port, want) in [("0x1F90", 8080), (" 8081 ", 8081)] {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        let agent_var = agent.display().to_string();
        let env = session_core::RunEnv {
            cwd: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
            var: std::sync::Arc::new(move |k| (k == config::paths::ENV_AGENT_DIR).then(|| agent_var.clone())),
            package: dir.path().to_path_buf(),
        };
        let args = [
            "mcp",
            "add",
            "s",
            "--url",
            "https://example.invalid/mcp",
            "--oauth-callback-port",
            port,
        ]
        .map(str::to_string);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::mcp_cli::run(&args, &env, &mut out, &mut err);
        assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&err));
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(agent.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(saved["mcpServers"]["s"]["oauth"]["callbackPort"], want, "{saved}");
    }
}
