//! The always registered `claude-code` provider through the `ultron` binary and the CLI's own views, against
//! `crates/ai/fixtures/claude-code-provider.json` (the TypeScript runtime recorded by
//! `crates/ai/fixtures/generate-claude-code-provider.mjs`, with and without a `claude` where the provider looks):
//!
//! - `ultron --list-models [search]` prints what TypeScript's `listModels` printed, plus the rows of the models
//!   added after the recording;
//! - `/login`'s Claude Code row carries the status `loginProviderOptions` showed;
//! - `--model claude-code/<id>` (and `--provider claude-code`) resolve for a session as `resolveCliModel` does, and
//!   a request on them always has auth (the CLI's own login).
//!
//! The `claude` files are inert stand-ins (`exit 97`) that nothing runs. The Rust-only `claude-subscription`
//! provider lists its models when Claude Code resolves; every run points `ULTRON_CLAUDE_COMMAND` at a missing file
//! so that only the TypeScript providers are listed. Private HOME, agent and server dirs; nothing leaves the machine.

use cli::Env;
use serde_json::Value;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../../ai/fixtures/claude-code-provider.json")).unwrap()
}

fn docs() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/coding-agent/docs")
        .canonicalize()
        .unwrap()
        .display()
        .to_string()
}

/// The generator's layout under `root`, with each case's agent dir (and models.json) at `case-<i>/agent`.
fn layout(root: &Path) {
    for (dir, name, mode) in [
        ("bin", "claude", 0o755),
        ("plain", "claude", 0o644),
        ("cmd", "claude.cmd", 0o755),
        ("home/.local/bin", "claude", 0o755),
    ] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        let file = root.join(dir).join(name);
        std::fs::write(&file, "#!/bin/sh\nexit 97\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    std::fs::create_dir_all(root.join("empty")).unwrap();
}

struct Case {
    spec: Value,
    agent: PathBuf,
    vars: HashMap<String, String>,
}

fn cases(root: &Path) -> Vec<Case> {
    layout(root);
    let expand = |text: &str| text.replace("<root>", &root.display().to_string());
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            let agent = root.join(format!("case-{i}/agent"));
            std::fs::create_dir_all(&agent).unwrap();
            if !spec["models"].is_null() {
                std::fs::write(agent.join("models.json"), spec["models"].to_string()).unwrap();
            }
            let mut vars: HashMap<String, String> = [
                ("HOME", expand("<root>/home")),
                (config::env::OFFLINE_ENV, "1".into()),
                (config::paths::ENV_AGENT_DIR, agent.display().to_string()),
                (
                    config::paths::ENV_SERVER_DIR,
                    root.join(format!("case-{i}/server")).display().to_string(),
                ),
                (
                    ai::claude_subscription::process::COMMAND_ENV,
                    expand("<root>/missing/claude"),
                ),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            for (k, v) in spec["env"].as_object().unwrap() {
                vars.insert(k.clone(), expand(v.as_str().unwrap()));
            }
            Case {
                spec: spec.clone(),
                agent,
                vars,
            }
        })
        .collect()
}

impl Case {
    fn name(&self) -> &str {
        self.spec["name"].as_str().unwrap()
    }

    fn env(&self, root: &Path) -> Env {
        let vars = self.vars.clone();
        Env {
            cwd: root.to_path_buf(),
            home: root.join("home"),
            var: Arc::new(move |k| vars.get(k).cloned()),
            package: PathBuf::from(docs()).join(".."),
        }
    }
}

/// The catalog models no recorded listing shows: `claude-fable-5-1`, which the Rust provider added after the
/// recording (cc8501ec) with the fixture's `catalogModels` and each case's models, but not its `list` output.
fn added_after_recording() -> Vec<String> {
    let fixture = fixture();
    let listed: Vec<&str> = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|case| case["list"].as_array().unwrap())
        .flat_map(|listing| listing["stdout"].as_str().unwrap().lines())
        .filter_map(|line| line.split_whitespace().nth(1))
        .collect();
    fixture["catalogModels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|model| model["id"].as_str().unwrap().to_string())
        .filter(|id| !listed.contains(&id.as_str()))
        .collect()
}

/// A recorded listing as it reads with the `added` models: when it shows every other claude-code model its case has
/// available (its search admitted the whole provider), each added model takes the row of a listed model with the
/// same catalog columns, under its own id, in id order.
fn with_added_models(case: &Value, stdout: &str, added: &[String]) -> String {
    let mut lines: Vec<String> = stdout.lines().map(str::to_string).collect();
    let (Some(start), Some(end)) = (lines[0].find("model"), lines[0].find("context")) else {
        return stdout.to_string();
    };
    let id_of = |line: &str| line[start..end].trim_end().to_string();
    let mut listed: Vec<String> = lines[1..]
        .iter()
        .filter(|line| line.starts_with("claude-code "))
        .map(|line| id_of(line))
        .collect();
    let available: Vec<&str> = case["available"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model.as_str()?.strip_prefix("claude-code/"))
        .collect();
    let mut others: Vec<&str> = available
        .iter()
        .copied()
        .filter(|id| !added.iter().any(|a| a == id))
        .collect();
    listed.sort();
    others.sort();
    if listed.is_empty() || listed != others {
        return stdout.to_string();
    }
    let columns = |id: &str| {
        let model = case["providerModels"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == id)
            .unwrap();
        ["contextWindow", "maxTokens", "reasoning", "input"].map(|key| model[key].clone())
    };
    for id in added.iter().filter(|id| available.contains(&id.as_str())) {
        let twin = listed
            .iter()
            .find(|listed| columns(listed) == columns(id))
            .unwrap_or_else(|| panic!("no listed model has the catalog columns of {id}"));
        let row = lines
            .iter()
            .find(|line| line.starts_with("claude-code ") && id_of(line) == *twin)
            .unwrap();
        assert!(id.len() < end - start, "{id} would widen the recorded model column");
        let row = format!("{}{id:<width$}{}", &row[..start], &row[end..], width = end - start);
        let at = (1..lines.len())
            .find(|&i| lines[i].starts_with("claude-code ") && id_of(&lines[i]) > *id)
            .unwrap_or(lines.len());
        lines.insert(at, row);
    }
    lines.iter().map(|line| format!("{line}\n")).collect()
}

/// `want` with `claude-fable-5-2`, which only the Rust catalog has (the fixture holds neither a listing nor a catalog
/// entry for it, and the ai crate's parity tests leave it out the same way): it lists as `claude-fable-5-1` does,
/// on the row after it.
fn with_rust_only_model(want: &str) -> String {
    let twin = |line: &str| line.starts_with("claude-code ") && line.contains(" claude-fable-5-1 ");
    if !want.lines().any(twin) {
        return want.to_string();
    }
    want.lines()
        .flat_map(|line| {
            let added = twin(line).then(|| line.replace("claude-fable-5-1", "claude-fable-5-2"));
            std::iter::once(line.to_string()).chain(added)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

#[test]
fn cli_claude_code_list_models_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let docs = docs();
    let added = added_after_recording();
    assert_eq!(added, ["claude-fable-5-1"]);
    for case in cases(&root) {
        for listing in case.spec["list"].as_array().unwrap() {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ultron"));
            command.arg("--list-models");
            if let Some(search) = listing["search"].as_str() {
                command.arg(search);
            }
            let output = command
                .env_clear()
                .env("ULTRON_HINDSIGHT_URL", "off")
                .env("ULTRON_MEMORANTADO_URL", "off")
                .envs(&case.vars)
                .env("NO_COLOR", "1")
                .current_dir(&root)
                .output()
                .unwrap();
            let label = format!("{}: {}", case.name(), listing["search"]);
            assert!(output.status.success(), "{label}: {output:?}");
            let stdout = String::from_utf8(output.stdout).unwrap().replace(&docs, "<docs>");
            let want = with_added_models(&case.spec, listing["stdout"].as_str().unwrap(), &added);
            assert_eq!(stdout, with_rust_only_model(&want), "{label}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert_eq!(stderr, listing["stderr"].as_str().unwrap(), "{label}");
        }
    }
}

#[test]
fn cli_claude_code_login_status_matches_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for case in cases(&root) {
        // A models.json entry for a built-in provider (any provider's) recomposes its `/login` method (a savable
        // key, the configured key's status) in TypeScript; the Rust `/login` lists the built-in method.
        if !case.spec["models"]["providers"]["claude-code"].is_null() {
            continue;
        }
        let env = case.env(&root);
        let options = cli::interactive::login_options(&env, &case.agent.join("auth.json"));
        let rows: Vec<_> = options.iter().filter(|o| o.id == "claude-code").collect();
        let want = case.spec["login"].as_array().unwrap();
        assert_eq!(rows.len(), want.len(), "{}", case.name());
        for (row, want) in rows.iter().zip(want) {
            assert_eq!(row.name, want["name"].as_str().unwrap());
            assert_eq!(row.auth_type, want["authType"].as_str().unwrap());
            let (method, can_login) = row.api_key_method.clone().unwrap();
            assert_eq!(method, want["method"].as_str().unwrap());
            assert_eq!(can_login, want["canLogin"].as_bool().unwrap());
            let status = row
                .status
                .as_ref()
                .map(|(kind, source)| serde_json::json!({"type": kind, "source": source}));
            assert_eq!(status.unwrap_or(Value::Null), want["status"], "{}", case.name());
        }
    }
}

#[test]
fn cli_claude_code_model_flags_resolve_for_sessions_like_typescript() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    for case in cases(&root) {
        let env = case.env(&root);
        for expected in case.spec["resolve"].as_array().unwrap() {
            let flags = session_core::ModelFlags {
                provider: expected["provider"].as_str().map(str::to_string),
                model: expected["model"].as_str().map(str::to_string),
                ..session_core::ModelFlags::default()
            };
            let label = format!("{}: {flags:?}", case.name());
            match (
                session_core::lane::resolve_choice(&flags, &env, false),
                expected["result"].as_str(),
            ) {
                (Ok(choice), Some(reference)) => {
                    assert_eq!(
                        format!("{}/{}", choice.model.provider, choice.model.id),
                        reference,
                        "{label}"
                    );
                    assert_eq!(choice.thinking.as_deref(), expected["thinking"].as_str(), "{label}");
                    assert_eq!(choice.warning.as_deref(), expected["warning"].as_str(), "{label}");
                    // `getAuth` resolves for every claude-code model, with or without the CLI.
                    if choice.model.provider == "claude-code" {
                        assert!(
                            session_core::lane::request_key(&flags, &env, "claude-code")
                                .unwrap()
                                .is_some(),
                            "{label}"
                        );
                    }
                }
                (Err(error), None) => assert_eq!(Some(error.as_str()), expected["error"].as_str(), "{label}"),
                (got, want) => panic!("{label}: got {:?}, TypeScript {want:?}", got.map(|c| c.model.id)),
            }
        }
    }
}
