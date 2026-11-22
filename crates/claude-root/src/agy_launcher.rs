//! `ultron agy [options] [agy args...]`: Antigravity CLI (`agy`) terminal with Ultron's REPL (`ultron mcp`) attached.
//! Installs/verifies Ultron's `ultron-mcp-relay.py` entry in `~/.gemini/config/mcp_config.json` (shared with
//! `ai::antigravity`), passes `ULTRON_AGY_MCP_EXEC` so the relay execs `ultron mcp --socket <socket>` directly, and
//! supplies a session-only `--settings` JSON file that allows `mcp(ultron/*)` (and denies native `command(*)` /
//! `write_file(*)` unless `--keep-native-tools` is passed).

use crate::control::socket_path_for;
use crate::launcher::base36;
use crate::self_cmd::{SelfCommand, shell_command};
use crate::{Ctx, UsageError};
use jsrt::string::shell_quote;
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const AGY_BIN_ENV: &str = "ULTRON_AGY_BIN";

pub const HELP: &str = "Usage: ultron agy [options] [agy args...]

Run Antigravity CLI (`agy`) in your terminal with Ultron's REPL (`rlm`) attached as its MCP server.

Options (before any agy args, or anywhere before --):
  --print-config         Print the agy command, env, and settings instead of running it
  --keep-native-tools    Also allow agy's native command/write_file tools (default: deny them so work goes through rlm)
  --no-context-files, -nc  Leave AGENTS.md / CLAUDE.md out of Ultron's MCP instructions and subagents
  --no-skills, -ns       Leave your skills out of Ultron's MCP instructions and subagents
  --watch                Open `ultron watch` in a tmux split (inside tmux)
  --frame-model <p/m>    Model of rlm.map/rlm.infer frames
  --children <mode>      rlm.spawn subagents: claude or ultron
  --child-model <alias>  Model of subagents
  --help                 This help

Everything else is passed to `agy` unchanged (for example -c, --conversation <id>, --model <id>, --effort high, -p \"prompt\").";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgyLauncherOptions {
    pub print_config: bool,
    pub keep_native_tools: bool,
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
    let mut options = AgyLauncherOptions::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || -> Result<String, UsageError> {
            let next = args
                .get(index + 1)
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))?;
            index += 1;
            Ok(next.clone())
        };
        match arg {
            "--" => {
                options.agy_args.extend(args[index + 1..].iter().cloned());
                break;
            }
            "--help" | "-h" => return Ok(None),
            "--print-config" => options.print_config = true,
            "--keep-native-tools" => options.keep_native_tools = true,
            "--watch" => options.watch = true,
            "--no-context-files" | "-nc" => options.no_context_files = true,
            "--no-skills" | "-ns" => options.no_skills = true,
            "--frame-model" => options.frame_model = Some(value()?),
            "--child-model" => options.child_model = Some(value()?),
            "--children" => {
                let mode = crate::settings::ChildrenMode::parse(&value()?);
                options.children = Some(mode.ok_or_else(|| UsageError("--children must be claude or ultron".into()))?);
            }
            other => options.agy_args.push(other.to_string()),
        }
        index += 1;
    }
    Ok(Some(options))
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

    let env = vec![(
        ai::antigravity::MCP_EXEC_ENV.to_string(),
        serde_json::to_string(&mcp_argv).expect("argv serializes"),
    )];

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
    let dir = if parsed.print_config {
        None
    } else {
        let path_env = var("PATH").unwrap_or_default();
        ai::antigravity::ensure_relay(&ctx.env.home, &path_env)
            .map_err(|e| format!("could not configure agy MCP relay: {e}"))?;
        Some({
            let base = config::paths::tmpdir(var);
            config::paths::private_temp_dir(&base, "ultron-agy-").map_err(|e| format!("{}: {e}", base.display()))?
        })
    };
    let launch = build_agy_launch(&agy, &self_cmd, &parsed, dir.as_deref(), &socket.display().to_string());
    let Some(dir) = dir else {
        let env_prefix: String = launch
            .env
            .iter()
            .map(|(k, v)| format!("{k}={} ", shell_quote(v)))
            .collect();
        let command = std::iter::once(agy.as_str())
            .chain(launch.args.iter().map(String::as_str))
            .map(shell_quote)
            .collect::<Vec<_>>()
            .join(" ");
        let printed = json!({
            "agy": agy,
            "args": launch.args,
            "env": launch.env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>(),
            "settings": launch.settings,
            "socket": socket.display().to_string(),
            "watch": launch.watch_command,
            "command": format!("{env_prefix}{command}"),
        });
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&printed).expect("JSON values serialize")
        )
        .map_err(|e| e.to_string())?;
        return Ok(0);
    };
    for (path, content) in &launch.files {
        config::paths::write_private(path, content).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let interactive = !parsed
        .agy_args
        .iter()
        .any(|a| a == "-p" || a == "--print" || a == "--prompt");
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
    Ok(exec_agy(&launch, &dir, err))
}

fn exec_agy(launch: &AgyLaunch, dir: &Path, err: &mut dyn std::io::Write) -> i32 {
    use std::os::unix::process::CommandExt;
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; rm -rf -- \"$2\"",
            "ultron-agy-cleanup",
            &pid,
            &dir.display().to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    let error = std::process::Command::new(&launch.agy)
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .exec();
    let _ = std::fs::remove_dir_all(dir);
    let _ = writeln!(err, "ultron agy: {error}");
    1
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
}
