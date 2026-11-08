//! `ultron claude|mcp|hook|guide` (claude/cli.ts): the dispatch main.ts does for these commands. `ultron watch`, the
//! terminal viewer of a running `ultron mcp`, is the cli crate's (it draws with the TUI); it reports failures with
//! [`exit_code`].

use crate::guide::{Audience, GuideOptions, ResourceFlags};
use crate::{Ctx, UsageError};
use session_core::RunEnv;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

pub const CLAUDE_COMMANDS: [&str; 4] = ["claude", "mcp", "hook", "guide"];

pub const GUIDE_HELP: &str =
    "Usage: ultron guide [--for ultron|claude|claude-child] [--cwd <dir>] [--no-context-files] [--no-skills]

Print the runtime guide: Ultron's own (default), or the system prompt `ultron claude` gives Claude Code
(--for claude; --for claude-child for a Claude Code subagent), with the directory's context files and skills.";

pub fn version() -> String {
    let package: serde_json::Value = serde_json::from_str(include_str!("../../../packages/coding-agent/package.json"))
        .expect("packages/coding-agent/package.json parses");
    package["version"].as_str().unwrap_or_default().to_string()
}

/// A command's failure: a usage error (exit 2) or a message classified like TypeScript's catch.
enum Failure {
    Usage(String),
    Other(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Other(message)
    }
}

impl From<UsageError> for Failure {
    fn from(error: UsageError) -> Self {
        Failure::Usage(error.0)
    }
}

/// `/^unknown option|needs a value|must be /`: messages that exit 2.
fn usage_message(message: &str) -> bool {
    message.starts_with("unknown option") || message.contains("needs a value") || message.contains("must be ")
}

fn guide(args: &[String], ctx: &Ctx, out: &mut dyn Write) -> Result<i32, Failure> {
    let mut audience = Audience::Ultron;
    let mut cwd: Option<String> = None;
    let mut flags = ResourceFlags::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--help" | "-h" => {
                writeln!(out, "{GUIDE_HELP}").map_err(|e| e.to_string())?;
                return Ok(0);
            }
            "--for" => {
                index += 1;
                audience = match args.get(index).map(String::as_str) {
                    Some("ultron") => Audience::Ultron,
                    Some("claude") => Audience::Claude,
                    Some("claude-child") => Audience::ClaudeChild,
                    _ => return Err(UsageError("--for must be ultron, claude or claude-child".into()).into()),
                };
            }
            "--cwd" => {
                index += 1;
                cwd = Some(
                    args.get(index)
                        .cloned()
                        .ok_or_else(|| UsageError("--cwd needs a directory".into()))?,
                );
            }
            "--no-context-files" | "-nc" => flags.no_context_files = true,
            "--no-skills" | "-ns" => flags.no_skills = true,
            other => return Err(UsageError(format!("unknown option for ultron guide: {other}")).into()),
        }
        index += 1;
    }
    let var = &*ctx.env.var;
    let hints = kernel_apis::cell_hints::hints_enabled(var(kernel_apis::cell_hints::HINTS_ENV).as_deref());
    let dir = cwd.unwrap_or_else(|| ctx.env.cwd.display().to_string());
    let bundled = ctx.bundled_skills();
    let resources;
    let apis;
    let options = if audience == Audience::Ultron {
        GuideOptions::default()
    } else {
        let env = RunEnv {
            cwd: PathBuf::from(&dir),
            ..ctx.env.clone()
        };
        // What `ultron claude` launches with: engineering mode's bundled-skills note is the proactive one, except
        // under `ULTRON_GUIDE=ts`, which keeps TypeScript's wording (load_prompt_resources decides both).
        resources = crate::guide::load_prompt_resources(&env, bundled.as_deref(), flags);
        apis = crate::guide::runtime_apis(&env);
        GuideOptions {
            cwd: Some(dir.clone()),
            platform: Some(crate::guide::platform().into()),
            date: Some(crate::guide::utc_date()),
            allowance: None,
            resources: Some(&resources),
            bundled_skills: bundled.as_deref(),
            wake: crate::guide::wakes(&*ctx.env.var, true),
            apis: Some(&apis),
        }
    };
    let text = crate::guide::runtime_guide(audience, &options, hints);
    writeln!(out, "{text}").map_err(|e| e.to_string())?;
    Ok(0)
}

fn dispatch(argv: &[String], ctx: &Ctx, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, Failure> {
    let rest = &argv[1..];
    match argv[0].as_str() {
        "guide" => guide(rest, ctx, out),
        "hook" => {
            let stdin = std::io::stdin();
            let tty = stdin.is_terminal();
            Ok(crate::hooks::run(rest, &mut stdin.lock(), tty, &*ctx.env.var, out, err))
        }
        "mcp" => {
            let stdin = std::io::stdin();
            Ok(crate::mcp_server::run(
                rest,
                ctx,
                &mut stdin.lock(),
                Box::new(std::io::stdout()),
            )?)
        }
        "claude" => Ok(crate::launcher::run(rest, ctx, out, err)?),
        other => Err(UsageError(format!("unknown command: {other}")).into()),
    }
}

/// Run one of [`CLAUDE_COMMANDS`] (`argv[0]` is the command); `None` when `argv` is not one of them.
pub fn run(
    argv: &[String],
    env: &RunEnv,
    session_report: crate::mcp_server::SessionReport,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<i32> {
    if !argv.first().is_some_and(|c| CLAUDE_COMMANDS.contains(&c.as_str())) {
        return None;
    }
    let ctx = Ctx {
        env: env.clone(),
        session_report: Some(session_report),
    };
    Some(report(dispatch(argv, &ctx, out, err), err))
}

fn report(result: Result<i32, Failure>, err: &mut dyn Write) -> i32 {
    match result {
        Ok(code) => code,
        Err(Failure::Usage(message)) => {
            let _ = writeln!(err, "Error: {message}");
            2
        }
        Err(Failure::Other(message)) => {
            let _ = writeln!(err, "Error: {message}");
            if usage_message(&message) { 2 } else { 1 }
        }
    }
}

/// The exit code of one of these commands' results, as claude/cli.ts's catch gives it: the code, or `Error: <message>`
/// on `err` and 2 for a usage message, else 1.
pub fn exit_code(result: Result<i32, String>, err: &mut dyn Write) -> i32 {
    report(result.map_err(Failure::Other), err)
}
