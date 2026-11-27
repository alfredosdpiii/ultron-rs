//! `ultron durable [attach] [options] [prompt]` (user requirement; no TypeScript equivalent): a client of the durable
//! instance. The TUI (no prompt), `-p` and `--mode json|rpc` run as for any instance, except that they use the
//! durable instance's detached server (activated, held open, when it is not running) instead of a server of their
//! own, and leave it running when they exit: its kept children and scheduled prompts go on. Project and explicit
//! instances keep their own foreground servers as before.
//!
//! The client runs with the durable instance as its explicit server directory and a marker variable in its own
//! (injected) environment; the server it activates is the one `ultron durable start` starts.

use crate::Env;
use daemon::server::{ServerOptions, activate_server};
use daemon::workers::WorkerModel;
use protocol::Route;
use std::io::Write;
use std::sync::Arc;

/// Set (to `1`) in the environment of a client of the durable instance.
pub const CLIENT_ENV: &str = "__ULTRON_DURABLE_CLIENT";

pub fn is_client(env: &Env) -> bool {
    (env.var)(CLIENT_ENV).as_deref() == Some("1")
}

/// The durable server's route for a client with `options`: activated (held open) when it is not running. Its
/// workers start on the default worker options; the client's own session options (`--model`, `--provider`,
/// `--api-key`, `--thinking`, `--models`, `--no-session`, `--approve`/`--no-approve`, `--system-prompt`,
/// `--append-system-prompt`, `--skill`, `--no-skills`, `--no-context-files`) go with its attach (crate::link), so
/// the worker its session starts runs on them as a foreground server's would. The client's run environment
/// (`--offline`, the `httpProxy` setting) belongs to the server process: a server this client activates runs with
/// it.
pub fn activate(options: &ServerOptions) -> Result<Route, String> {
    activate_server(
        &ServerOptions {
            model: WorkerModel {
                env: options.model.env.clone(),
                ..WorkerModel::default()
            },
            keep_alive: true,
            relay: None,
            ..options.clone()
        },
        true,
    )
    .map_err(|e| e.to_string())
}

/// The command herdr runs to resume `session_id` opened through `ultron durable`: `ultron durable --session <id>`
/// (a client of the durable server again, never a foreground server on its directory), behind `env VAR=...` for
/// the instance or agent directory the caller's environment chose.
pub fn durable_resume_argv(env: &Env, session_id: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let assigned: Vec<String> = [config::paths::ENV_SERVER_DIR, config::paths::ENV_AGENT_DIR]
        .iter()
        .filter_map(|k| (env.var)(k).filter(|v| !v.is_empty()).map(|v| format!("{k}={v}")))
        .collect();
    if !assigned.is_empty() {
        argv.push("env".to_string());
        argv.extend(assigned);
    }
    argv.extend(["ultron", "durable", "--session", session_id].map(String::from));
    argv
}

/// Run `args` (an ordinary `ultron` command line) as a client of the durable instance.
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let caller = env.clone();
    crate::herdr_report::set_resume_override(move |session_id| Some(durable_resume_argv(&caller, session_id)));
    let instance = crate::instance(env, true).dir;
    let own = daemon::instance::ultron_home(&env.home).join("server");
    let sessions = (config::paths::normalize(&instance) == config::paths::normalize(&own)).then(|| "1".to_string());
    let dir = instance.display().to_string();
    let inner = env.var.clone();
    let client = Env {
        var: Arc::new(move |key| match key {
            CLIENT_ENV => Some("1".into()),
            config::settings::DURABLE_CLIENT_ENV => sessions.clone(),
            config::paths::ENV_SERVER_DIR => Some(dir.clone()),
            config::paths::LEGACY_ENV_SERVER_DIR => None,
            _ => inner(key),
        }),
        ..env.clone()
    };
    Ok(crate::run(args, &client, out, err))
}

/// The binary's name that runs [`run_home`] (`ultron-s`, a link to `ultron`).
pub const HOME_COMMAND: &str = "ultron-s";
/// The durable instance's home session id, in its directory.
pub const HOME_FILE: &str = "home-session";

const HOME_USAGE: &str = "Usage: ultron-s [--new | --session <id> [--home] | --session-id <id> | -c | -r | --fork <id> | --list] [ultron options] [prompt]";

/// Whether the binary was invoked as `ultron-s` (its file name; a path or a `.exe` suffix does not matter).
pub fn is_home_command(argv0: &str) -> bool {
    let name = std::path::Path::new(argv0)
        .file_name()
        .map_or_default(|n| n.to_string_lossy().into_owned());
    name.strip_suffix(".exe").unwrap_or(&name) == HOME_COMMAND
}

pub fn home_session(instance_dir: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(instance_dir.join(HOME_FILE))
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|id| !id.is_empty())
}

/// The durable instance's directory when this process is `ultron-s` ([`run_home`]): its TUI's new sessions can move
/// the home pointer ([`follow_home`]).
static HOME_INSTANCE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The TUI started session `new` in place of `old` (`/new`, `/clear`): under `ultron-s`, when `old` is the home
/// session, `new` becomes it, so a later bare `ultron-s` opens it. Another session (`ultron-s --session <id>`) leaves
/// the pointer alone; the old session stays on disk either way.
pub fn follow_home(old: &str, new: &str) -> Result<(), String> {
    match HOME_INSTANCE.get() {
        Some(dir) if home_session(dir).as_deref() == Some(old) => set_home(dir, new),
        _ => Ok(()),
    }
}

/// herdr resumes the pane through `ultron-s` (the home session, as recorded when it asks) or
/// `ultron-s --session <id>`.
fn resume_through_home(instance_dir: &std::path::Path) {
    let dir = instance_dir.to_path_buf();
    crate::herdr_report::set_resume_override(move |session_id| {
        Some(home_resume_argv(home_session(&dir).as_deref(), session_id))
    });
}

fn set_home(instance_dir: &std::path::Path, id: &str) -> Result<(), String> {
    jsrt::fs::atomic_write(&instance_dir.join(HOME_FILE), format!("{id}\n")).map_err(|e| e.to_string())
}

/// `ultron-s [--new | --session <id> [--home] | --session-id <id> | -c | -r | --fork <id> | --list] [options]
/// [prompt]`: the durable instance's home session (one long-lived session) as a client of its server: the TUI, `-p`
/// or `--mode json|rpc`, run in the home directory. The home session is created (and recorded) when there is none or
/// the recorded one was deleted; `--new` makes a fresh one home; `--session <id>` uses another session of the
/// instance (`--home` records it as home); `--session-id <id>` opens that session, created when missing; `-c` the
/// instance's most recently active session; `-r` its session picker; `--fork <id>` a new session forked from one of
/// them; `--list` lists them. Exiting leaves the server and any run in progress going.
pub fn run_home(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match home_inner(args, env, out, err) {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(err, "Error: {error}");
            1
        }
    }
}

/// The command herdr runs to resume `session_id` opened through `ultron-s`: `ultron-s` for the home session, else
/// `ultron-s --session <id>`.
pub fn home_resume_argv(home: Option<&str>, session_id: &str) -> Vec<String> {
    let mut argv = vec![HOME_COMMAND.to_string()];
    if home != Some(session_id) {
        argv.extend(["--session".to_string(), session_id.to_string()]);
    }
    argv
}

#[derive(Default)]
struct HomeArgs {
    new: bool,
    list: bool,
    make_home: bool,
    session: Option<String>,
    selector: Option<Vec<String>>,
    rest: Vec<String>,
}

fn set_home_selector(slot: &mut Option<Vec<String>>, value: Vec<String>) -> Result<(), String> {
    if slot.is_some() {
        return Err(HOME_USAGE.into());
    }
    *slot = Some(value);
    Ok(())
}

fn consume_selector_flag(args: &[String], i: &mut usize, slot: &mut Option<Vec<String>>) -> Result<bool, String> {
    match args[*i].as_str() {
        flag @ ("-c" | "--continue" | "-r" | "--resume") => {
            set_home_selector(slot, vec![flag.to_string()])?;
            Ok(true)
        }
        flag @ ("--fork" | "--session-id") => {
            *i += 1;
            let value = args.get(*i).cloned().ok_or(HOME_USAGE)?;
            set_home_selector(slot, vec![flag.to_string(), value])?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn consume_home_flag(args: &[String], i: &mut usize, parsed: &mut HomeArgs) -> Result<(), String> {
    if consume_selector_flag(args, i, &mut parsed.selector)? {
        return Ok(());
    }
    match args[*i].as_str() {
        "--new" => parsed.new = true,
        "--list" => parsed.list = true,
        "--home" => parsed.make_home = true,
        "--session" => {
            *i += 1;
            parsed.session = Some(args.get(*i).cloned().ok_or(HOME_USAGE)?);
        }
        _ => parsed.rest.push(args[*i].clone()),
    }
    Ok(())
}

fn validate_home_args(parsed: &HomeArgs) -> Result<(), String> {
    let has_session = parsed.session.is_some();
    if parsed.make_home && !has_session {
        return Err(HOME_USAGE.into());
    }
    let modes = usize::from(parsed.new)
        + usize::from(parsed.list)
        + usize::from(has_session)
        + usize::from(parsed.selector.is_some());
    if modes > 1 {
        return Err(HOME_USAGE.into());
    }
    Ok(())
}

fn parse_home_args(args: &[String]) -> Result<HomeArgs, String> {
    let mut parsed = HomeArgs::default();
    let mut i = 0;
    while i < args.len() {
        consume_home_flag(args, &mut i, &mut parsed)?;
        i += 1;
    }
    validate_home_args(&parsed)?;
    Ok(parsed)
}

fn list_home_sessions(
    mut sessions: Vec<daemon::catalog::Meta>,
    home: Option<&str>,
    out: &mut dyn Write,
) -> Result<i32, String> {
    sessions.sort_by(|a, b| b.modified_at.total_cmp(&a.modified_at));
    writeln!(out, "ID\tNAME\tLAST ACTIVITY\tHOME").map_err(|e| e.to_string())?;
    for meta in &sessions {
        let name = daemon::catalog::details(meta).name.unwrap_or_else(|| "-".into());
        let at = jsrt::date::iso_string(meta.modified_at as i64);
        let mark = if home == Some(meta.id.as_str()) { "*" } else { "" };
        writeln!(out, "{}\t{name}\t{at}\t{mark}", meta.id).map_err(|e| e.to_string())?;
    }
    Ok(0)
}

fn resolve_continue_selector(
    mut selector: Vec<String>,
    instance_dir: &std::path::Path,
    session_dir: &std::path::Path,
    mut sessions: Vec<daemon::catalog::Meta>,
) -> Vec<String> {
    if selector[0] == "-c" || selector[0] == "--continue" {
        let dream = daemon::dream::session_id(instance_dir, session_dir);
        sessions.retain(|m| Some(&m.id) != dream.as_ref());
        sessions.sort_by(|a, b| b.modified_at.total_cmp(&a.modified_at));
        selector = match sessions.first() {
            Some(latest) => vec!["--session-id".to_string(), latest.id.clone()],
            None => Vec::new(),
        };
    }
    selector
}

fn resolve_home_session_id(
    parsed: &HomeArgs,
    home: Option<String>,
    sessions: &[daemon::catalog::Meta],
    instance_dir: &std::path::Path,
) -> Result<String, String> {
    if let Some(id) = &parsed.session {
        if !sessions.iter().any(|m| m.id == *id) {
            return Err(format!("Unknown session: {id}"));
        }
        if parsed.make_home {
            set_home(instance_dir, id)?;
        }
        return Ok(id.clone());
    }
    match home.filter(|_| !parsed.new) {
        Some(id) => Ok(id),
        None => {
            let id = jsrt::random::uuid_v4();
            set_home(instance_dir, &id)?;
            Ok(id)
        }
    }
}

fn run_at_home(
    mut args: Vec<String>,
    rest: &[String],
    instance_dir: &std::path::Path,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    resume_through_home(instance_dir);
    args.extend(caller_args(rest, &env.cwd, &env.home));
    let at_home = Env {
        cwd: env.home.clone(),
        ..env.clone()
    };
    run(&args, &at_home, out, err)
}

fn home_inner(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let parsed = parse_home_args(args)?;
    let instance = crate::instance(env, true);
    instance.prepare().map_err(|e| e.to_string())?;
    let _ = HOME_INSTANCE.set(instance.dir.clone());
    let session_dir = crate::session::session_directory_of(None, env, &instance);
    let sessions = daemon::catalog::list(&session_dir);
    let home = home_session(&instance.dir).filter(|id| sessions.iter().any(|m| &m.id == id));
    if parsed.list {
        return list_home_sessions(sessions, home.as_deref(), out);
    }
    if let Some(selector) = parsed.selector {
        let selector = resolve_continue_selector(selector, &instance.dir, &session_dir, sessions);
        return run_at_home(selector, &parsed.rest, &instance.dir, env, out, err);
    }
    let id = resolve_home_session_id(&parsed, home, &sessions, &instance.dir)?;
    let client_args = vec!["--session-id".to_string(), id];
    run_at_home(client_args, &parsed.rest, &instance.dir, env, out, err)
}

/// Flags whose value is the next argument (so an `@...` value is not a file argument).
const VALUE_FLAGS: [&str; 24] = [
    "--provider",
    "--model",
    "--api-key",
    "--system-prompt",
    "--name",
    "-n",
    "--session",
    "--session-id",
    "--fork",
    "--session-dir",
    "--models",
    "--tools",
    "-t",
    "--exclude-tools",
    "-xt",
    "--thinking",
    "--export",
    "--extension",
    "-e",
    "--prompt-template",
    "--theme",
    "--mode",
    "--use-theme",
    "--tui-mode",
];

fn resolve_at_arg(arg: &str, cwd: &std::path::Path, home: &std::path::Path) -> String {
    match arg.strip_prefix('@') {
        Some(path) => format!("@{}", crate::file_args::resolve_read_path(path, cwd, home).display()),
        None => arg.to_string(),
    }
}

fn resolve_append_prompt(value: &str, cwd: &std::path::Path) -> String {
    let path = cwd.join(value);
    if !value.is_empty() && path.exists() {
        path.display().to_string()
    } else {
        value.to_string()
    }
}

/// `args` with what names a file of the caller's directory `cwd` made absolute, for a run in another directory:
/// `@file` arguments, `--skill <path>`, and an `--append-system-prompt` value that is a file there (else its text,
/// unchanged).
pub fn caller_args(args: &[String], cwd: &std::path::Path, home: &std::path::Path) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match (arg, args.get(i + 1)) {
            ("--", _) => {
                out.push(arg.to_string());
                out.extend(args[i + 1..].iter().map(|a| resolve_at_arg(a, cwd, home)));
                break;
            }
            ("--append-system-prompt", Some(value)) => {
                out.extend([arg.to_string(), resolve_append_prompt(value, cwd)]);
                i += 1;
            }
            ("--skill", Some(value)) => {
                let path = crate::file_args::resolve_read_path(value, cwd, home);
                out.extend([arg.to_string(), path.display().to_string()]);
                i += 1;
            }
            (flag, Some(value)) if VALUE_FLAGS.contains(&flag) => {
                out.extend([arg.to_string(), value.clone()]);
                i += 1;
            }
            _ => out.push(resolve_at_arg(arg, cwd, home)),
        }
        i += 1;
    }
    out
}
