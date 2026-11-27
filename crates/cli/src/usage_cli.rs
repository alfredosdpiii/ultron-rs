//! `ultron usage [session-id|path] [--last N] [--json]` (`ultron/usage-cli.ts`): the session report of a session
//! file, or a table of the most recent sessions. It only reads session files: no model call, no server, nothing
//! written.

use crate::Env;
use jsrt::localtime::Zone;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use usage::log::{LogError, read_session_header, read_session_log};
use usage::{report, text};

pub const SESSION_REPORT_LIST_SCHEMA: &str = "ultron.session-report-list/1";

fn usage() -> String {
    [
        "Usage: ultron usage [session-id | session.jsonl] [options]",
        "",
        "What a session did: turns, cells, depth (frames, sub-agents, workflows), tokens and cost per lane and model,",
        "guardrails and memory decisions. Reads session files only: no model call, no server, nothing written.",
        "",
        "With no session, reports the most recent session of the current directory.",
        "",
        "Options:",
        "  --last <N>              A table of the N most recent sessions, across all directories",
        "  --here                  With --last: only sessions of the current directory",
        "  --all                   With --last: include sessions in which nothing ran",
        "  --json                  The report as JSON (schema ultron.session-report/1; with --last, a list)",
        "  --utc                   Print times in UTC",
        "  --sessions-root <dir>   Where sessions are kept (default: the profile's sessions directory)",
    ]
    .join("\n")
}

#[derive(Debug, Default)]
struct Options {
    target: Option<String>,
    last: Option<u32>,
    here: bool,
    all: bool,
    json: bool,
    utc: bool,
    sessions_root: Option<String>,
}

/// A usage error (exit 2, usage printed) or a failure (exit 1).
enum Failure {
    Usage(String),
    Error(String),
}

/// `Number(text)` for the `--last` value: a safe integer or nothing.
fn js_integer(text: &str) -> Option<f64> {
    Some(jsrt::number::parse(text)).filter(|n| jsrt::number::is_safe_integer(*n))
}

fn option_value(args: &[String], index: &mut usize, name: &str) -> Result<String, Failure> {
    let arg = &args[*index];
    let inline = match arg.find('=') {
        Some(at) => Some(arg[at + 1..].to_string()),
        None => {
            *index += 1;
            args.get(*index).cloned()
        }
    };
    match inline {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(Failure::Usage(format!("Option {name} requires a value"))),
    }
}

fn parse_last_count(raw: &str) -> Result<u32, Failure> {
    match js_integer(raw) {
        Some(n) if (1.0..=1000.0).contains(&n) => Ok(n as u32),
        _ => Err(Failure::Usage("--last needs a number between 1 and 1000".into())),
    }
}

fn apply_bool_flag(arg: &str, options: &mut Options) -> bool {
    match arg {
        "--json" => options.json = true,
        "--here" => options.here = true,
        "--all" => options.all = true,
        "--utc" => options.utc = true,
        _ => return false,
    }
    true
}

fn is_flag_with_value(arg: &str, name: &str) -> bool {
    arg == name || arg.strip_prefix(name).is_some_and(|rest| rest.starts_with('='))
}

fn apply_arg(args: &[String], index: &mut usize, options: &mut Options) -> Result<(), Failure> {
    let arg = &args[*index];
    if apply_bool_flag(arg, options) {
        return Ok(());
    }
    if is_flag_with_value(arg, "--last") {
        let raw = option_value(args, index, "--last")?;
        options.last = Some(parse_last_count(&raw)?);
        return Ok(());
    }
    if is_flag_with_value(arg, "--sessions-root") {
        options.sessions_root = Some(option_value(args, index, "--sessions-root")?);
        return Ok(());
    }
    if arg.starts_with('-') {
        return Err(Failure::Usage(format!("Unknown option {arg}")));
    }
    set_target(options, arg)
}

fn set_target(options: &mut Options, arg: &str) -> Result<(), Failure> {
    if options.target.is_some() {
        return Err(Failure::Usage(format!("Unexpected argument: {arg}")));
    }
    options.target = Some(arg.to_string());
    Ok(())
}

fn validate_options(options: &Options) -> Result<(), Failure> {
    if options.target.is_some() && options.last.is_some() {
        return Err(Failure::Usage("Give a session or --last, not both".into()));
    }
    if (options.here || options.all) && options.last.is_none() {
        return Err(Failure::Usage("--here and --all go with --last".into()));
    }
    Ok(())
}

fn parse(args: &[String]) -> Result<Options, Failure> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        apply_arg(args, &mut index, &mut options)?;
        index += 1;
    }
    validate_options(&options)?;
    Ok(options)
}

struct SessionFile {
    path: PathBuf,
    modified_at: f64,
}

/// Every session file under the sessions root (one directory per working directory), newest first.
fn list_session_files(root: &Path) -> Vec<SessionFile> {
    let mut files: Vec<SessionFile> = session_journal::catalog::session_files(root)
        .into_iter()
        .filter_map(|path| {
            let modified_at = std::fs::metadata(&path)
                .ok()?
                .modified()
                .map_or(0.0, jsrt::date::mtime_ms);
            Some(SessionFile { path, modified_at })
        })
        .collect();
    files.sort_by(|a, b| {
        b.modified_at
            .partial_cmp(&a.modified_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| jsrt::collate::locale_compare(&a.path.to_string_lossy(), &b.path.to_string_lossy()))
    });
    files
}

/// The session id in a session file's name (`<created>_<id>.jsonl`).
fn file_session_id(path: &Path) -> String {
    let text = path.to_string_lossy();
    let name = text.rsplit('/').next().unwrap_or(&text);
    let name = name.strip_suffix(".jsonl").unwrap_or(name);
    name.split_once('_').map_or(name, |(_, id)| id).to_string()
}

/// The Ultron session of a Claude Code session (`ultron claude` remembers it by Claude Code's session id).
fn claude_session_path(agent_dir: &Path, id: &str) -> Option<PathBuf> {
    let path = config::paths::AgentPaths::new(agent_dir).claude_session_map(id);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let target = PathBuf::from(saved.get("path")?.as_str()?);
    target.exists().then_some(target)
}

fn resolve_target(target: &str, root: &Path, cwd: &Path, agent_dir: &Path) -> Result<PathBuf, String> {
    let as_path = config::paths::normalize(&cwd.join(target));
    if std::fs::metadata(&as_path).is_ok_and(|m| m.is_file()) {
        return Ok(as_path);
    }
    let files = list_session_files(root);
    let exact: Vec<&SessionFile> = files.iter().filter(|f| file_session_id(&f.path) == target).collect();
    let matches = if exact.is_empty() {
        files
            .iter()
            .filter(|f| file_session_id(&f.path).starts_with(target))
            .collect()
    } else {
        exact
    };
    if matches.len() == 1 {
        return Ok(matches[0].path.clone());
    }
    if matches.len() > 1 {
        let shown: Vec<String> = matches.iter().take(5).map(|f| file_session_id(&f.path)).collect();
        return Err(format!(
            "{} sessions match \"{target}\": {}{}",
            matches.len(),
            shown.join(", "),
            if matches.len() > 5 { ", ..." } else { "" }
        ));
    }
    if let Some(path) = claude_session_path(agent_dir, target) {
        return Ok(path);
    }
    Err(format!(
        "No session file or session id \"{target}\" (looked in {})",
        root.display()
    ))
}

/// A session's report, with the costs of `subscription` providers (a subscription login) as notional.
fn report_of(path: &Path, subscription: &[String]) -> Result<(Value, bool), LogError> {
    let log = read_session_log(path)?;
    Ok((
        report::build_session_report(&log, subscription),
        report::session_is_empty(&log),
    ))
}

/// Handle `usage ...`; returns None when `args` is not a usage command, else the exit code.
pub fn run(args: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if args.first().map(String::as_str) != Some("usage") {
        return None;
    }
    let rest = &args[1..];
    let mut lines: Vec<String> = Vec::new();
    let code = if rest.iter().any(|a| a == "--help" || a == "-h") {
        lines.push(usage());
        0
    } else {
        match command(rest, env, &mut lines) {
            Ok(()) => 0,
            Err(Failure::Usage(message)) => {
                let _ = writeln!(err, "Error: {message}\n\n{}", usage());
                2
            }
            Err(Failure::Error(message)) => {
                let _ = writeln!(err, "Error: {message}");
                1
            }
        }
    };
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    Some(code)
}

fn log_failure(e: LogError) -> Failure {
    Failure::Error(e.message().to_string())
}

fn load_candidate_report(
    path: &Path,
    here: bool,
    cwd: &str,
    subscription: &[String],
) -> Result<Option<(Value, bool)>, LogError> {
    if here && read_session_header(path)?.cwd != cwd {
        return Ok(None);
    }
    report_of(path, subscription).map(Some)
}

fn collect_last_reports(
    last: u32,
    options: &Options,
    root: &Path,
    cwd: &str,
    subscription: &[String],
) -> Result<(Vec<Value>, usize), Failure> {
    let mut reports = Vec::new();
    let mut skipped = 0;
    for file in list_session_files(root) {
        if reports.len() >= last as usize {
            break;
        }
        match load_candidate_report(&file.path, options.here, cwd, subscription) {
            Ok(None) | Err(LogError::Session(_)) => {}
            Ok(Some((_, true))) if !options.all => skipped += 1,
            Ok(Some((report, _))) => reports.push(report),
            Err(e) => return Err(log_failure(e)),
        }
    }
    Ok((reports, skipped))
}

fn render_last_reports(
    reports: &[Value],
    skipped: usize,
    options: &Options,
    root: &Path,
    cwd: &str,
    text: &text::TextOptions<'_>,
    lines: &mut Vec<String>,
) {
    if options.json {
        lines.push(jsrt::json::stringify_pretty(&json!({
            "schema": SESSION_REPORT_LIST_SCHEMA,
            "sessions": reports,
        })));
        return;
    }
    if reports.is_empty() {
        let scope = if options.here {
            format!(" of {cwd}")
        } else {
            String::new()
        };
        let omitted = if skipped > 0 {
            format!(" ({skipped} with nothing run; --all lists them)")
        } else {
            String::new()
        };
        lines.push(format!("No sessions{scope} under {}{omitted}", root.display()));
        return;
    }
    lines.extend(text::render_session_table(reports, text));
    if skipped > 0 {
        let suffix = if skipped == 1 { "" } else { "s" };
        lines.push(format!(
            "({skipped} newer session{suffix} in which nothing ran not listed; --all lists them)"
        ));
    }
}

fn latest_session_in_cwd(root: &Path, cwd: &str) -> Result<PathBuf, Failure> {
    let mut fallback = None;
    for file in list_session_files(root) {
        if !read_session_header(&file.path).is_ok_and(|h| h.cwd == cwd) {
            continue;
        }
        fallback.get_or_insert_with(|| file.path.clone());
        if !report::session_is_empty(&read_session_log(&file.path).map_err(log_failure)?) {
            return Ok(file.path);
        }
    }
    fallback.ok_or_else(|| {
        Failure::Error(format!(
            "No session of {cwd} under {}; name one, or see `ultron usage --last 10`",
            root.display()
        ))
    })
}

fn command(rest: &[String], env: &Env, lines: &mut Vec<String>) -> Result<(), Failure> {
    let options = parse(rest)?;
    let agent_dir = env.agent_dir();
    let root = config::paths::session_root(
        options.sessions_root.as_deref(),
        &*env.var,
        &config::paths::AgentPaths::new(&agent_dir).experimental_sessions_dir(),
        &env.cwd,
        &env.home,
    );
    let zone = Zone::from_env((env.var)("TZ").as_deref());
    let home = env.home.display().to_string();
    let text = text::TextOptions {
        utc: options.utc,
        home: &home,
        zone: &zone,
    };
    let cwd = env.cwd.display().to_string();
    let subscription = session_core::lane::subscription_providers(env);
    if let Some(last) = options.last {
        let (reports, skipped) = collect_last_reports(last, &options, &root, &cwd, &subscription)?;
        render_last_reports(&reports, skipped, &options, &root, &cwd, &text, lines);
        return Ok(());
    }
    let path = match &options.target {
        Some(target) => resolve_target(target, &root, &env.cwd, &agent_dir).map_err(Failure::Error)?,
        None => latest_session_in_cwd(&root, &cwd)?,
    };
    let (report, _) = report_of(&path, &subscription).map_err(log_failure)?;
    if options.json {
        lines.push(jsrt::json::stringify_pretty(&report));
    } else {
        lines.extend(text::render_session_report(&report, &text));
    }
    Ok(())
}
