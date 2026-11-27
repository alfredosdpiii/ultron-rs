//! `ultron claude [options] [claude args...]` (launcher.ts): Claude Code as the root agent of Ultron. It finds
//! `claude`, checks the login with `claude auth status --json` (it never touches credential files), checks the flags
//! it needs, and runs Claude Code with Ultron's REPL as its one MCP server (`--strict-mcp-config --mcp-config`), no
//! built-in tools (`--tools "" --allowedTools mcp__ultron__rlm`), the Claude guide as its system prompt, no user or
//! project Claude Code settings (`--setting-sources ""`), Ultron's hooks (`--settings`) and
//! `--dangerously-skip-permissions` (see [`crate::claude_cli::skip_permissions_args`]). Other arguments go to
//! `claude` unchanged. `--print-config` prints the command instead of running it, with the configs inline.

use crate::claude_cli::{self, ClaudeAuthStatus, REQUIRED_CLAUDE_FLAGS, resolve_claude_binary};
use crate::control::socket_path_for;
use crate::guide::{
    Audience, CLAUDE_RLM_TOOL, GuideOptions, PromptResources, ResourceFlags, claude_system_prompt,
    describe_prompt_resources,
};
use crate::hooks::{HERDR_HOOKS, HookEvent, SESSION_HOOKS};
use crate::self_cmd::{SelfCommand, shell_command};
use crate::{Ctx, UsageError};
use config::providers::CLAUDE_CODE_DEFAULT_MODEL;
use jsrt::string::shell_quote;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const HELP: &str = "Usage: ultron claude [options] [claude args...]

Run Claude Code as the root agent of Ultron: its only tool is Ultron's REPL (rlm).

Options (before any claude args, or anywhere before --):
  --print-config         Print the claude command and its config instead of running it
  --keep-mcp             Also load your own Claude Code MCP servers (default: only Ultron's)
  --keep-settings        Load your user/project Claude Code settings and CLAUDE.md (default: none)
  --no-hooks             No Ultron hooks (no automatic memory, events only in rlm results)
  --no-context-files, -nc  Leave AGENTS.md / CLAUDE.md out of the system prompt (included by default, as in ultron)
  --no-skills, -ns       Leave your skills out of the system prompt (listed by default, as in ultron)
  --watch                Open `ultron watch` in a tmux split (inside tmux)
  --frame-model <p/m>    Model of rlm.map/rlm.infer frames (default claude-code/claude-opus-5-5)
  --children <mode>      rlm.spawn subagents: claude (Claude Code processes, default) or ultron
  --child-model <alias>  Claude Code model of claude subagents (default claude-opus-5-5)
  --help                 This help

Claude Code itself runs claude-opus-5-5 unless you pass --model (or set ULTRON_CLAUDE_MODEL).
Everything else is passed to claude (for example -p \"prompt\", --model sonnet, --resume <id>).";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LauncherOptions {
    pub print_config: bool,
    pub keep_mcp: bool,
    pub keep_settings: bool,
    pub watch: bool,
    pub hooks: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
    pub frame_model: Option<String>,
    pub children: Option<crate::settings::ChildrenMode>,
    pub child_model: Option<String>,
    /// Claude Code model of the root when neither `--model` nor ULTRON_CLAUDE_MODEL names one (`claudeCode.model`).
    pub root_model: Option<String>,
    pub claude_args: Vec<String>,
}

pub(crate) fn next_arg_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, UsageError> {
    let next = args
        .get(*index + 1)
        .ok_or_else(|| UsageError(format!("{flag} needs a value")))?;
    *index += 1;
    Ok(next.clone())
}

pub(crate) fn parse_children_mode(
    args: &[String],
    index: &mut usize,
) -> Result<crate::settings::ChildrenMode, UsageError> {
    let raw = next_arg_value(args, index, "--children")?;
    crate::settings::ChildrenMode::parse(&raw).ok_or_else(|| UsageError("--children must be claude or ultron".into()))
}

pub(crate) fn format_launch_command(bin: &str, args: &[String], env: &[(String, String)]) -> String {
    let env_prefix: String = env.iter().map(|(k, v)| format!("{k}={} ", shell_quote(v))).collect();
    let command = std::iter::once(bin)
        .chain(args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    format!("{env_prefix}{command}")
}

pub(crate) fn env_json_map(env: &[(String, String)]) -> Map<String, Value> {
    env.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()
}

fn apply_launcher_bool_flag(arg: &str, options: &mut LauncherOptions) -> bool {
    match arg {
        "--print-config" => options.print_config = true,
        "--keep-mcp" => options.keep_mcp = true,
        "--keep-settings" => options.keep_settings = true,
        "--watch" => options.watch = true,
        "--no-hooks" => options.hooks = false,
        "--no-context-files" | "-nc" => options.no_context_files = true,
        "--no-skills" | "-ns" => options.no_skills = true,
        _ => return false,
    }
    true
}

fn apply_launcher_value_flag(
    arg: &str,
    args: &[String],
    index: &mut usize,
    options: &mut LauncherOptions,
) -> Result<bool, UsageError> {
    match arg {
        "--frame-model" => options.frame_model = Some(next_arg_value(args, index, arg)?),
        "--child-model" => options.child_model = Some(next_arg_value(args, index, arg)?),
        "--children" => options.children = Some(parse_children_mode(args, index)?),
        _ => return Ok(false),
    }
    Ok(true)
}

/// Split `ultron claude` arguments into Ultron's options and claude's; `None` for `--help`.
pub fn parse_launcher_args(args: &[String]) -> Result<Option<LauncherOptions>, UsageError> {
    let mut options = LauncherOptions {
        hooks: true,
        ..LauncherOptions::default()
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            options.claude_args.extend(args[index + 1..].iter().cloned());
            break;
        }
        if matches!(arg, "--help" | "-h") {
            return Ok(None);
        }
        if !apply_launcher_bool_flag(arg, &mut options)
            && !apply_launcher_value_flag(arg, args, &mut index, &mut options)?
        {
            options.claude_args.push(arg.to_string());
        }
        index += 1;
    }
    Ok(Some(options))
}

/// Everything `claude` is started with.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeLaunch {
    pub claude: String,
    pub args: Vec<String>,
    /// Environment added to the process's own.
    pub env: Vec<(String, String)>,
    /// Files the args refer to, by path, in the order written.
    pub files: Vec<(PathBuf, String)>,
    pub socket: String,
    pub watch_command: String,
    /// The system prompt's length in UTF-16 code units (JS `length`).
    pub prompt_chars: usize,
}

/// What [`build_claude_launch`] needs.
pub struct LaunchInput<'a> {
    pub claude: &'a str,
    pub flags: &'a BTreeSet<String>,
    pub self_cmd: &'a SelfCommand,
    pub options: &'a LauncherOptions,
    pub cwd: &'a str,
    /// The session's private temp directory (files inline when `None`).
    pub dir: Option<&'a Path>,
    pub socket: &'a str,
    pub var: &'a dyn Fn(&str) -> Option<String>,
    pub date: String,
    pub platform: &'a str,
    pub resources: &'a PromptResources,
    pub bundled_skills: Option<&'a Path>,
    pub hints: bool,
    pub skip_permissions: Vec<String>,
    /// The kernel APIs the guide names (crate::guide::runtime_apis); none: the TypeScript guide's set.
    pub apis: Option<&'a resources::runtime_guide::RuntimeGuideOptions>,
}

/// The Claude Code model the root runs on: a `--model` among the claude args (`None`: it is there, and goes to
/// `claude` as given), else ULTRON_CLAUDE_MODEL, `claudeCode.model`, the default.
fn root_model(options: &LauncherOptions, var: &dyn Fn(&str) -> Option<String>) -> (String, bool) {
    let args = &options.claude_args;
    let named = args
        .iter()
        .enumerate()
        .find_map(|(index, arg)| match arg.strip_prefix("--model=") {
            Some(value) => Some(value.to_string()),
            None => (arg == "--model").then(|| args.get(index + 1).cloned().unwrap_or_default()),
        });
    match named {
        Some(model) => (model, true),
        None => (
            var(config::env::CLAUDE_MODEL_ENV)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .or_else(|| options.root_model.clone())
                .unwrap_or_else(|| CLAUDE_CODE_DEFAULT_MODEL.into()),
            false,
        ),
    }
}

fn build_claude_mcp_config(input: &LaunchInput, root: Option<&String>, wake_hooks: bool) -> String {
    let options = input.options;
    let self_cmd = input.self_cmd;
    let mut mcp_args: Vec<String> = self_cmd.args.clone();
    mcp_args.extend(["mcp", "--no-instructions", "--socket", input.socket].map(String::from));
    let children = options.children.map(|mode| mode.as_str().to_string());
    for (flag, value) in [
        ("--frame-model", options.frame_model.as_ref()),
        ("--children", children.as_ref()),
        ("--child-model", options.child_model.as_ref()),
        ("--root-model", root),
    ] {
        if let Some(value) = value {
            mcp_args.extend([flag.to_string(), value.clone()]);
        }
    }
    if options.no_context_files {
        mcp_args.push("--no-context-files".into());
    }
    if options.no_skills {
        mcp_args.push("--no-skills".into());
    }
    if crate::guide::wakes(input.var, wake_hooks) {
        mcp_args.push("--wake".into());
    }
    jsrt::json::stringify(&json!({
        "mcpServers": {"ultron": {"type": "stdio", "command": self_cmd.command, "args": mcp_args}}
    }))
}

fn build_claude_settings(input: &LaunchInput, wake_hooks: bool) -> String {
    let command =
        |event: HookEvent| shell_command(input.self_cmd, &["hook", event.command(), "--socket", input.socket]);
    let hook = |event: HookEvent| json!({"hooks": [{"type": "command", "command": command(event), "timeout": event.timeout_secs()}]});
    let mut settings = Map::new();
    settings.insert("permissions".into(), json!({"allow": [CLAUDE_RLM_TOOL]}));
    if input.options.hooks {
        let mut hooks = json!({});
        for event in SESSION_HOOKS {
            hooks[event.claude_event()] = json!([hook(event)]);
        }
        if crate::guide::wakes(input.var, wake_hooks) {
            let wait = HookEvent::WaitEvents;
            hooks[wait.claude_event()] = json!([{"matcher": CLAUDE_RLM_TOOL, "hooks": [{
                "type": "command",
                "command": command(wait),
                "async": true,
                "asyncRewake": true,
                "timeout": wait.timeout_secs(),
            }]}]);
        }
        if herdr::detect(input.var, None).is_some() {
            for event in HERDR_HOOKS {
                hooks[event.claude_event()] = json!([hook(event)]);
            }
        }
        settings.insert("hooks".into(), hooks);
    }
    jsrt::json::stringify(&Value::Object(settings))
}

fn build_claude_cli_args(
    input: &LaunchInput,
    mcp_arg: String,
    settings_arg: String,
    prompt_arg: Result<String, String>,
    model: Option<String>,
) -> Vec<String> {
    let options = input.options;
    let mut args: Vec<String> = Vec::new();
    if !options.keep_mcp {
        args.push("--strict-mcp-config".into());
    }
    args.extend(["--mcp-config".into(), mcp_arg]);
    args.extend(["--tools", "", "--allowedTools", CLAUDE_RLM_TOOL].map(String::from));
    match prompt_arg {
        Ok(path) => args.extend(["--system-prompt-file".into(), path]),
        Err(inline) => args.extend(["--system-prompt".into(), inline]),
    }
    args.extend(["--settings".into(), settings_arg]);
    if !options.keep_settings {
        args.extend(["--setting-sources".into(), String::new()]);
    }
    if input.flags.contains("dangerously-skip-permissions") {
        args.extend(input.skip_permissions.iter().cloned());
    }
    if let Some(model) = model {
        args.extend(["--model".into(), model]);
    }
    args.extend(options.claude_args.iter().cloned());
    args
}

fn build_claude_env(var: &dyn Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if var("MCP_TOOL_TIMEOUT").is_none_or(|v| v.is_empty()) {
        env.push(("MCP_TOOL_TIMEOUT".to_string(), (2 * 60 * 60 * 1000).to_string()));
    }
    if var("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS").is_none_or(|v| v.is_empty()) {
        env.push(("CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS".to_string(), "0".to_string()));
    }
    env
}

/// `buildClaudeLaunch`. Unlike TypeScript, the server is told the root's model (`--root-model`, when it is not the
/// default): Ultron children that name no model run on it (crate::mcp_server::subscription_model).
pub fn build_claude_launch(input: &LaunchInput) -> ClaudeLaunch {
    let options = input.options;
    let (model, model_in_args) = root_model(options, input.var);
    let root = (model.trim() != CLAUDE_CODE_DEFAULT_MODEL).then(|| model.trim().to_string());
    let print = options.claude_args.iter().any(|a| a == "-p" || a == "--print");
    let wake_hooks = options.hooks && !print;
    let mcp_config = build_claude_mcp_config(input, root.as_ref(), wake_hooks);
    let settings = build_claude_settings(input, wake_hooks);
    let prompt = claude_system_prompt(
        Audience::Claude,
        &GuideOptions {
            cwd: Some(input.cwd.to_string()),
            platform: Some(input.platform.to_string()),
            date: Some(input.date.clone()),
            allowance: None,
            resources: Some(input.resources),
            bundled_skills: input.bundled_skills,
            wake: crate::guide::wakes(input.var, wake_hooks),
            apis: input.apis,
        },
        input.hints,
    );
    let mut files: Vec<(PathBuf, String)> = Vec::new();
    let mut file = |name: &str, content: &str| -> Option<String> {
        let path = input.dir?.join(name);
        files.push((path.clone(), content.to_string()));
        Some(path.display().to_string())
    };
    let mcp_path = file("mcp.json", &mcp_config);
    let settings_path = file("settings.json", &settings);
    let prompt_path = input
        .flags
        .contains("system-prompt-file")
        .then(|| file("system-prompt.md", &prompt))
        .flatten();
    let prompt_chars = prompt.encode_utf16().count();
    let prompt_arg = prompt_path.ok_or(prompt);
    let args = build_claude_cli_args(
        input,
        mcp_path.unwrap_or(mcp_config),
        settings_path.unwrap_or(settings),
        prompt_arg,
        (!model_in_args).then_some(model),
    );
    ClaudeLaunch {
        claude: input.claude.to_string(),
        args,
        env: build_claude_env(input.var),
        files,
        socket: input.socket.to_string(),
        watch_command: shell_command(input.self_cmd, &["watch", "--socket", input.socket]),
        prompt_chars,
    }
}

/// `requireClaude`: the binary and its flags, or why `ultron claude` cannot use it.
fn require_claude(var: &dyn Fn(&str) -> Option<String>) -> Result<(String, BTreeSet<String>), String> {
    let claude = resolve_claude_binary(var).ok_or(
        "Claude Code (`claude`) is not on PATH. Install it (https://claude.com/claude-code) or set ULTRON_CLAUDE_BIN.",
    )?;
    let flags = claude_cli::claude_flags(&claude);
    let missing: Vec<String> = REQUIRED_CLAUDE_FLAGS
        .iter()
        .filter(|f| !flags.contains(**f))
        .map(|f| format!("--{f}"))
        .collect();
    let version = claude_cli::claude_version(&claude);
    if !missing.is_empty() {
        return Err(format!(
            "{claude}{} lacks {}; update Claude Code (`claude update`).",
            version.map_or_default(|v| format!(" {v}")),
            missing.join(", ")
        ));
    }
    Ok((claude, flags))
}

/// A launch id: `L-<ms base 36>-<8 hex>`.
fn launch_id() -> String {
    format!(
        "L-{}-{}",
        base36(jsrt::date::now_ms() as u64),
        jsrt::random::random_hex(4)
    )
}

pub fn base36(mut n: u64) -> String {
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

fn print_claude_config(
    out: &mut dyn std::io::Write,
    claude: &str,
    auth: &ClaudeAuthStatus,
    launch: &ClaudeLaunch,
    resources: &PromptResources,
    socket: &Path,
) -> Result<i32, String> {
    let mut system_prompt = Map::new();
    system_prompt.insert("chars".into(), launch.prompt_chars.into());
    if let Value::Object(described) = describe_prompt_resources(resources) {
        system_prompt.extend(described);
    }
    let printed = json!({
        "claude": claude,
        "auth": {"loggedIn": auth.logged_in, "method": auth.describe()},
        "args": launch.args,
        "env": env_json_map(&launch.env),
        "socket": socket.display().to_string(),
        "watch": launch.watch_command,
        "systemPrompt": system_prompt,
        "command": format_launch_command(claude, &launch.args, &launch.env),
    });
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&printed).expect("JSON values serialize")
    )
    .map_err(|e| e.to_string())?;
    Ok(0)
}

fn create_claude_launch_dir(
    var: &dyn Fn(&str) -> Option<String>,
    print_config: bool,
) -> Result<Option<PathBuf>, String> {
    if print_config {
        return Ok(None);
    }
    let base = config::paths::tmpdir(var);
    let dir =
        config::paths::private_temp_dir(&base, "ultron-claude-").map_err(|e| format!("{}: {e}", base.display()))?;
    Ok(Some(dir))
}

fn write_files_and_exec(
    parsed: &LauncherOptions,
    self_cmd: &SelfCommand,
    launch: &ClaudeLaunch,
    dir: &Path,
    var: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<i32, String> {
    for (path, content) in &launch.files {
        config::paths::write_private(path, content).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let interactive = !parsed.claude_args.iter().any(|a| a == "-p" || a == "--print");
    open_watch_pane(
        parsed.watch,
        interactive,
        self_cmd,
        &launch.socket,
        &launch.watch_command,
        var,
        err,
    )?;
    let _ = err.flush();
    let _ = out.flush();
    Ok(exec_claude(launch, dir, err))
}

fn require_authenticated_claude(
    var: &dyn Fn(&str) -> Option<String>,
    print_config: bool,
) -> Result<(String, BTreeSet<String>, ClaudeAuthStatus), String> {
    let (claude, flags) = require_claude(var)?;
    let auth: ClaudeAuthStatus = claude_cli::claude_auth_status(&claude)?;
    if !auth.logged_in && !print_config {
        return Err("Claude Code is not logged in. Run `claude auth login` (or `claude`, then /login) first.".into());
    }
    Ok((claude, flags, auth))
}

/// `ultron claude ...`; returns the exit code (or replaces this process with Claude Code).
pub fn run(
    argv: &[String],
    ctx: &Ctx,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> Result<i32, String> {
    let Some(mut parsed) = parse_launcher_args(argv).map_err(|e| e.0)? else {
        writeln!(out, "{HELP}").map_err(|e| e.to_string())?;
        return Ok(0);
    };
    let var = &*ctx.env.var;
    let claude_settings = crate::settings::ClaudeCodeSettings::load(&ctx.env.cwd, &ctx.env.agent_dir());
    if parsed.root_model.is_none() {
        parsed.root_model = claude_settings.model.clone();
    }
    let (claude, flags, auth) = require_authenticated_claude(var, parsed.print_config)?;
    let self_cmd = crate::self_cmd::self_command(var)?;
    let cwd = ctx.env.cwd.display().to_string();
    let socket = socket_path_for(&launch_id(), &crate::control::control_dir(var)?);
    let bundled = ctx.bundled_skills();
    let resources = crate::guide::load_prompt_resources(
        &ctx.env,
        bundled.as_deref(),
        ResourceFlags {
            no_context_files: parsed.no_context_files,
            no_skills: parsed.no_skills,
        },
    );
    let skip = claude_cli::skip_permissions_args(var, claude_cli::current_uid(), claude_settings.skip_permissions);
    let apis = crate::guide::runtime_apis(&ctx.env);
    let dir = create_claude_launch_dir(var, parsed.print_config)?;
    let launch = build_claude_launch(&LaunchInput {
        claude: &claude,
        flags: &flags,
        self_cmd: &self_cmd,
        options: &parsed,
        cwd: &cwd,
        dir: dir.as_deref(),
        socket: &socket.display().to_string(),
        var,
        date: crate::guide::utc_date(),
        platform: jsrt::process::platform(),
        resources: &resources,
        bundled_skills: bundled.as_deref(),
        hints: session_core::compose::hints(var),
        skip_permissions: skip,
        apis: Some(&apis),
    });
    match dir {
        None => print_claude_config(out, &claude, &auth, &launch, &resources, &socket),
        Some(dir) => write_files_and_exec(&parsed, &self_cmd, &launch, &dir, var, out, err),
    }
}

/// Opens `ultron watch` in a tmux split or `herdr-app` right pane when `--watch` is requested, else prints the watch
/// command to stderr.
pub(crate) fn open_watch_pane(
    watch: bool,
    interactive: bool,
    self_cmd: &SelfCommand,
    socket: &str,
    watch_command: &str,
    var: &dyn Fn(&str) -> Option<String>,
    err: &mut dyn std::io::Write,
) -> Result<(), String> {
    if watch && var("TMUX").is_some_and(|v| !v.is_empty()) {
        let _ = std::process::Command::new("tmux")
            .args(["split-window", "-h", "-d", watch_command])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        return Ok(());
    }
    if watch && var("HC_TERMINAL_ID").is_some_and(|v| !v.trim().is_empty()) {
        let mut watch_argv = vec![self_cmd.command.clone()];
        watch_argv.extend(self_cmd.args.iter().cloned());
        watch_argv.extend(["watch".into(), "--socket".into(), socket.to_string()]);
        let argv_json = serde_json::to_string(&watch_argv).expect("argv serializes");
        let opened = std::process::Command::new("herdr-app")
            .args(["do", "terminal.open", "--place", "right", "--argv", &argv_json])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if opened {
            return Ok(());
        }
    }
    if interactive || watch {
        writeln!(err, "Ultron RLM view: {watch_command}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Replace this process with Claude Code (so Ctrl-C and the exit status are Claude Code's own, as TypeScript gets by
/// ignoring SIGINT while it waits), after starting a detached helper that removes the session's temp directory
/// once this process id is gone. Returns only when Claude Code could not be started.
fn exec_claude(launch: &ClaudeLaunch, dir: &Path, err: &mut dyn std::io::Write) -> i32 {
    use std::os::unix::process::CommandExt;
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 1; done; rm -rf -- \"$2\"",
            "ultron-claude-cleanup",
            &pid,
            &dir.display().to_string(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn();
    let error = std::process::Command::new(&launch.claude)
        .args(&launch.args)
        .envs(launch.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .exec();
    let _ = std::fs::remove_dir_all(dir);
    let _ = writeln!(err, "ultron claude: {error}");
    1
}
