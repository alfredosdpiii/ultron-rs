//! `ultron agy [options] [agy args...]`: Antigravity CLI (`agy`) terminal with Ultron's REPL (`ultron mcp`) attached.
//! Installs/verifies Ultron's `ultron-mcp-relay.py` entry in `~/.gemini/config/mcp_config.json` (shared with
//! `ai::antigravity`), passes `ULTRON_AGY_MCP_EXEC` so the relay execs `ultron mcp --socket <socket>` directly, and
//! supplies a session-only `--settings` JSON file that allows `mcp(ultron/*)` (and denies native `command(*)` /
//! `write_file(*)` unless `--keep-native-tools` is passed).

use crate::control::socket_path_for;
use crate::launcher::base36;
use crate::self_cmd::{SelfCommand, shell_command};
use crate::{Ctx, UsageError};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const AGY_BIN_ENV: &str = "ULTRON_AGY_BIN";
pub const AGY_HOOK_SOCKET_ENV: &str = "ULTRON_AGY_HOOK_SOCKET";
pub const AGY_HOOK_BIN_ENV: &str = "ULTRON_AGY_HOOK_BIN";

pub const HELP: &str = "Usage: ultron agy [options] [agy args...]

Run Antigravity CLI (`agy`) in your terminal with Ultron's REPL (`rlm`) attached as its MCP server.

Options (before any agy args, or anywhere before --):
  --print-config         Print the agy command, env, and settings instead of running it
  --keep-native-tools    Also allow agy's native command/write_file tools (default: deny them so work goes through rlm)
  --no-hooks             No Ultron hooks (no automatic memory recall / session-start hooks)
  --no-context-files, -nc  Leave AGENTS.md / CLAUDE.md out of Ultron's MCP instructions and subagents
  --no-skills, -ns       Leave your skills out of Ultron's MCP instructions and subagents
  --watch                Open `ultron watch` in a tmux or Herdr split
  --frame-model <p/m>    Model of rlm.map/rlm.infer frames
  --children <mode>      rlm.spawn subagents: claude or ultron
  --child-model <alias>  Model of subagents
  --help                 This help

Everything else is passed to `agy` unchanged (for example -c, --conversation <id>, --model <id>, --effort high, -p \"prompt\").";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgyLauncherOptions {
    pub print_config: bool,
    pub keep_native_tools: bool,
    pub hooks: bool,
    pub watch: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
    pub frame_model: Option<String>,
    pub children: Option<crate::settings::ChildrenMode>,
    pub child_model: Option<String>,
    pub agy_args: Vec<String>,
}

/// Split `ultron agy` arguments into Ultron's options and `agy`'s; `None` for `--help`.
pub fn parse_agy_args(args: &[String]) -> Result<Option<AgyLauncherOptions>, UsageError> {
    let mut keep_native_tools = false;
    let Some(common) = crate::launcher::parse_common_args(args, |arg| {
        if arg == "--keep-native-tools" {
            keep_native_tools = true;
            true
        } else {
            false
        }
    })?
    else {
        return Ok(None);
    };
    Ok(Some(AgyLauncherOptions {
        print_config: common.print_config,
        keep_native_tools,
        hooks: common.hooks,
        watch: common.watch,
        no_context_files: common.no_context_files,
        no_skills: common.no_skills,
        frame_model: common.frame_model,
        children: common.children,
        child_model: common.child_model,
        agy_args: common.passthrough_args,
    }))
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgyLaunch {
    pub agy: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub files: Vec<(PathBuf, String)>,
    pub settings: Value,
    pub socket: String,
    pub watch_command: String,
}

pub fn build_agy_launch(
    agy: &str,
    self_cmd: &SelfCommand,
    options: &AgyLauncherOptions,
    dir: Option<&Path>,
    socket: &str,
) -> AgyLaunch {
    let mut mcp_argv: Vec<String> = Vec::with_capacity(self_cmd.args.len() + 12);
    mcp_argv.push(self_cmd.command.clone());
    mcp_argv.extend(self_cmd.args.iter().cloned());
    mcp_argv.extend(["mcp", "--socket", socket].map(String::from));
    let children = options.children.map(|mode| mode.as_str().to_string());
    for (flag, value) in [
        ("--frame-model", &options.frame_model),
        ("--children", &children),
        ("--child-model", &options.child_model),
    ] {
        if let Some(value) = value {
            mcp_argv.extend([flag.to_string(), value.clone()]);
        }
    }
    if options.no_context_files {
        mcp_argv.push("--no-context-files".into());
    }
    if options.no_skills {
        mcp_argv.push("--no-skills".into());
    }

    let mut permissions = Map::new();
    permissions.insert("allow".into(), json!(["mcp(ultron/*)"]));
    if !options.keep_native_tools {
        permissions.insert("deny".into(), json!(["command(*)", "write_file(*)"]));
    }
    let settings = json!({ "permissions": permissions });
    let settings_json = jsrt::json::stringify(&settings);

    let mut files = Vec::new();
    let settings_arg = match dir {
        Some(dir) => {
            let path = dir.join("settings.json");
            files.push((path.clone(), settings_json));
            path.display().to_string()
        }
        None => settings_json,
    };

    let mut args = vec!["--settings".into(), settings_arg];
    args.extend(options.agy_args.iter().cloned());

    let mut env = vec![(
        ai::antigravity::MCP_EXEC_ENV.to_string(),
        serde_json::to_string(&mcp_argv).expect("argv serializes"),
    )];
    if options.hooks {
        env.push((AGY_HOOK_SOCKET_ENV.to_string(), socket.to_string()));
        env.push((AGY_HOOK_BIN_ENV.to_string(), self_cmd.command.clone()));
    }

    AgyLaunch {
        agy: agy.to_string(),
        args,
        env,
        files,
        settings,
        socket: socket.to_string(),
        watch_command: shell_command(self_cmd, &["watch", "--socket", socket]),
    }
}

fn read_json_or_empty_object(file: &Path) -> std::io::Result<Value> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error),
    };
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).map_err(|e| std::io::Error::other(format!("{} is not valid JSON: {e}", file.display())))
}

fn build_agy_ultron_hooks() -> Value {
    let mut ultron_hooks = Map::new();
    for event in crate::hooks::SESSION_HOOKS {
        let Some(agy_event) = event.agy_event() else {
            continue;
        };
        let cmd = format!(
            "sh -c 'test -n \"${AGY_HOOK_SOCKET_ENV}\" \
             && exec \"${AGY_HOOK_BIN_ENV}\" hook {} --socket \"${AGY_HOOK_SOCKET_ENV}\" || exit 0'",
            event.command()
        );
        ultron_hooks.insert(
            agy_event.into(),
            json!([{"type": "command", "command": cmd, "timeout": event.timeout_secs()}]),
        );
    }
    Value::Object(ultron_hooks)
}

/// Installs an env-guarded `ultron` entry in `<home>/.gemini/config/hooks.json` so `ultron agy` gets Ultron's
/// `SessionStart`, `PreInvocation`, and `Stop` hooks while plain `agy` runs remain unaffected.
pub fn ensure_agy_hooks(home: &Path) -> std::io::Result<()> {
    let config_dir = home.join(".gemini/config");
    std::fs::create_dir_all(&config_dir)?;
    let file = config_dir.join("hooks.json");
    let mut root = read_json_or_empty_object(&file)?;
    let entry = build_agy_ultron_hooks();
    let map = root
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other(format!("{} is not a JSON object", file.display())))?;
    if map.get("ultron") != Some(&entry) {
        map.insert("ultron".into(), entry);
        let mut bytes = serde_json::to_vec_pretty(&root)?;
        bytes.push(b'\n');
        jsrt::fs::atomic_write(&file, &bytes)?;
    }
    Ok(())
}

fn resolve_agy_binary(var: &dyn Fn(&str) -> Option<String>, home: &Path) -> Result<String, String> {
    if let Some(explicit) = var(AGY_BIN_ENV).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) {
        return Ok(explicit);
    }
    let path = var("PATH").unwrap_or_default();
    ai::antigravity::catalog::resolve_agy(&path, Some(home))
        .map(|p| p.display().to_string())
        .ok_or_else(|| {
            "Antigravity CLI (`agy`) is not on PATH. Install it (https://antigravity.google/docs/install) or set ULTRON_AGY_BIN."
                .to_string()
        })
}

fn launch_id() -> String {
    format!(
        "A-{}-{}",
        base36(jsrt::date::now_ms() as u64),
        jsrt::random::random_hex(4)
    )
}

fn prepare_agy_launch_dir(
    var: &dyn Fn(&str) -> Option<String>,
    home: &Path,
    parsed: &AgyLauncherOptions,
) -> Result<Option<PathBuf>, String> {
    if parsed.print_config {
        return Ok(None);
    }
    let path_env = var("PATH").unwrap_or_default();
    ai::antigravity::ensure_relay(home, &path_env).map_err(|e| format!("could not configure agy MCP relay: {e}"))?;
    if parsed.hooks {
        ensure_agy_hooks(home).map_err(|e| format!("could not configure agy hooks: {e}"))?;
    }
    let base = config::paths::tmpdir(var);
    let dir = config::paths::private_temp_dir(&base, "ultron-agy-").map_err(|e| format!("{}: {e}", base.display()))?;
    Ok(Some(dir))
}

fn print_agy_config(out: &mut dyn std::io::Write, agy: &str, launch: &AgyLaunch, socket: &Path) -> Result<i32, String> {
    let printed = json!({
        "agy": agy,
        "args": launch.args,
        "env": crate::launcher::env_json_map(&launch.env),
        "settings": launch.settings,
        "socket": socket.display().to_string(),
        "watch": launch.watch_command,
        "command": crate::launcher::format_launch_command(agy, &launch.args, &launch.env),
    });
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&printed).expect("JSON values serialize")
    )
    .map_err(|e| e.to_string())?;
    Ok(0)
}

pub fn run(
    argv: &[String],
    ctx: &Ctx,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<i32, String> {
    let Some(parsed) = parse_agy_args(argv).map_err(|e| e.0)? else {
        writeln!(out, "{HELP}").map_err(|e| e.to_string())?;
        return Ok(0);
    };
    let var = &*ctx.env.var;
    let agy = resolve_agy_binary(var, &ctx.env.home)?;
    let self_cmd = crate::self_cmd::self_command(var)?;
    let socket = socket_path_for(&launch_id(), &crate::control::control_dir(var)?);
    let dir = prepare_agy_launch_dir(var, &ctx.env.home, &parsed)?;
    let launch = build_agy_launch(&agy, &self_cmd, &parsed, dir.as_deref(), &socket.display().to_string());
    let Some(dir) = dir else {
        return print_agy_config(out, &agy, &launch, &socket);
    };
    crate::launcher::write_launch_files(&launch.files)?;
    let interactive = !parsed
        .agy_args
        .iter()
        .any(|a| matches!(a.as_str(), "-p" | "--print" | "--prompt"));
    crate::launcher::open_watch_pane(
        parsed.watch,
        interactive,
        &self_cmd,
        &launch.socket,
        &launch.watch_command,
        var,
        err,
    )?;
    let _ = err.flush();
    let _ = out.flush();
    Ok(crate::launcher::exec_with_cleanup(
        &crate::launcher::ExecSpec {
            label: "ultron agy",
            cleanup_name: "ultron-agy-cleanup",
            program: &launch.agy,
            args: &launch.args,
            env: &launch.env,
        },
        &dir,
        err,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_agy_args_splits_ultron_and_agy_flags() {
        let args: Vec<String> = [
            "--watch",
            "--keep-native-tools",
            "--frame-model",
            "groq/llama-3.1-8b-instant",
            "-c",
            "--effort",
            "high",
            "--",
            "--help",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let parsed = parse_agy_args(&args).unwrap().unwrap();
        assert!(parsed.watch);
        assert!(parsed.keep_native_tools);
        assert_eq!(parsed.frame_model.as_deref(), Some("groq/llama-3.1-8b-instant"));
        assert_eq!(parsed.agy_args, vec!["-c", "--effort", "high", "--help"]);
    }

    #[test]
    fn build_agy_launch_sets_mcp_exec_and_session_permissions() {
        let self_cmd = SelfCommand {
            command: "/usr/local/bin/ultron".into(),
            args: vec![],
        };
        let options = AgyLauncherOptions {
            no_skills: true,
            agy_args: vec!["--model".into(), "gemini-4-neon-medium".into()],
            ..Default::default()
        };
        let launch = build_agy_launch("/usr/bin/agy", &self_cmd, &options, None, "/tmp/u.sock");
        assert_eq!(
            launch.settings,
            json!({
                "permissions": {
                    "allow": ["mcp(ultron/*)"],
                    "deny": ["command(*)", "write_file(*)"],
                }
            })
        );
        assert_eq!(launch.args[0], "--settings");
        assert_eq!(&launch.args[2..], &["--model", "gemini-4-neon-medium"]);
        let (key, val) = &launch.env[0];
        assert_eq!(key, ai::antigravity::MCP_EXEC_ENV);
        let argv: Vec<String> = serde_json::from_str(val).unwrap();
        assert_eq!(
            argv,
            vec!["/usr/local/bin/ultron", "mcp", "--socket", "/tmp/u.sock", "--no-skills"]
        );
    }

    #[test]
    fn keep_native_tools_omits_deny_rules() {
        let self_cmd = SelfCommand {
            command: "/usr/local/bin/ultron".into(),
            args: vec![],
        };
        let options = AgyLauncherOptions {
            keep_native_tools: true,
            ..Default::default()
        };
        let launch = build_agy_launch("/usr/bin/agy", &self_cmd, &options, None, "/tmp/u.sock");
        assert_eq!(
            launch.settings,
            json!({ "permissions": { "allow": ["mcp(ultron/*)"] } })
        );
    }

    #[test]
    fn agy_hooks_use_pre_invocation() {
        let hooks = build_agy_ultron_hooks();
        let obj = hooks.as_object().expect("hooks object");
        assert!(obj.contains_key("SessionStart"));
        assert!(obj.contains_key("PreInvocation"));
        assert!(obj.contains_key("Stop"));
        assert!(!obj.contains_key("UserPromptSubmit"));
        for event in ["SessionStart", "PreInvocation", "Stop"] {
            let handlers = obj[event].as_array().expect("flat handler array");
            assert_eq!(handlers.len(), 1);
            assert_eq!(handlers[0]["type"], "command");
            assert!(handlers[0].get("hooks").is_none());
        }
    }
}
