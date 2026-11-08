//! `ultron skills proposals|show|approve|reject` (Rust-only): procedure skills proposed by dreaming or a session
//! (`skills.propose_procedure` in a cell, delegation::procedure::proposals) wait in `<agent dir>/skill-proposals/`
//! until the user publishes one into `<agent dir>/skills/<name>/SKILL.md`. Nothing is published without this
//! command. `approve` and `reject` are the user's: they refuse inside a kernel (`ULTRON_IN_KERNEL=1`, kernel::env)
//! and without a terminal on stdin, and they show the proposal with its sha256 and ask before acting. `approve`
//! publishes only the content it showed: an edit after that is refused (delegation::procedure::proposals::approve).

use crate::Env;
use delegation::procedure::proposals;
use std::io::{BufRead, IsTerminal, Write};

const USAGE: &str = "Usage: ultron skills proposals | show <id> | approve <id> | reject <id> [reason] (see --help)";

pub const HELP: &str = "\
Usage: ultron skills <command>

Procedure skills proposed by dreaming or a session, waiting for your approval.

Commands:
  proposals            List them (tab-separated: ID STATE NAME SEGMENTS EVIDENCE).
  show <id>            Print a proposal's provenance, its SKILL.md and that file's sha256.
  approve <id>         Show it and, once you confirm, publish it as ~/.ultron/agent/skills/<name>/SKILL.md (never
                       over an existing skill). Edit the proposal's SKILL.md first if you want changes; it is
                       checked again here, and a change after it was shown is refused.
  reject <id> [reason] Show it and, once you confirm, keep it out; the proposal stays on record.

approve and reject are yours: they ask at a terminal, and refuse without one or inside an Ultron kernel (a
model's cell).
";

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// A command's outcome: its text on stdout (exit 0), or its error on stderr (exit 1).
fn done(result: Result<String, String>, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    match result {
        Ok(text) => writeln!(out, "{text}").map(|()| 0).map_err(io),
        Err(error) => writeln!(err, "Error: {error}").map(|()| 1).map_err(io),
    }
}

/// `ultron skills ...`; `None` when `args` is not a skills command (a prompt that starts with "skills"). The user's
/// answers come from stdin when it is a terminal.
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    let stdin = std::io::stdin();
    let mut terminal = stdin.is_terminal().then(|| stdin.lock());
    run_with(
        args,
        env,
        terminal.as_mut().map(|lock| lock as &mut dyn BufRead),
        out,
        err,
    )
}

/// A proposal as `show` prints it: its record, its SKILL.md and that file's sha256.
fn shown(record: &serde_json::Value, content: &str) -> (String, String) {
    let sha = jsrt::hash::sha256_hex(content.as_bytes());
    let pretty = serde_json::to_string_pretty(record).unwrap_or_default();
    (format!("{pretty}\n\n{content}\nsha256: {sha}"), sha)
}

/// [`run`] with the user's `terminal` (`None`: stdin is not one, and `approve`/`reject` refuse).
pub fn run_with(
    args: &[String],
    env: &Env,
    terminal: Option<&mut dyn BufRead>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<Result<i32, String>> {
    let paths = config::paths::AgentPaths::new(env.agent_dir());
    let dir = paths.skill_proposals();
    let (verb, id, reason) = match args {
        [verb, id, reason @ ..] if verb == "approve" && reason.is_empty() || verb == "reject" => {
            (verb.as_str(), id.as_str(), reason)
        }
        _ => return reading(args, &dir, out, err),
    };
    let refuse = |why: &str, err: &mut dyn Write| {
        Some(
            writeln!(err, "Error: skills {verb} is the user's decision; {why}.")
                .map(|()| 1)
                .map_err(io),
        )
    };
    if kernel::env::inside_kernel(|name| (env.var)(name)) {
        return refuse("it does not run inside an Ultron kernel", err);
    }
    let Some(terminal) = terminal else {
        return refuse("run it at a terminal (stdin is not one)", err);
    };
    // Show the proposal as it is now, ask, and act on exactly what was shown.
    let (record, content) = match proposals::show(&dir, id) {
        Ok(found) => found,
        Err(error) => return Some(done(Err(error), out, err)),
    };
    let (text, sha) = shown(&record, &content);
    let question = format!(
        "{verb} skill proposal {id} ({}, sha256 {})? Type yes to confirm: ",
        record["name"].as_str().unwrap_or_default(),
        &sha[..12]
    );
    if let Err(e) = write!(out, "{text}\n\n{question}").and_then(|()| out.flush()) {
        return Some(Err(io(e)));
    }
    let mut answer = String::new();
    if let Err(e) = terminal.read_line(&mut answer) {
        return Some(Err(io(e)));
    }
    if !matches!(answer.trim(), "yes" | "y") {
        return Some(writeln!(err, "Not confirmed; nothing changed.").map(|()| 1).map_err(io));
    }
    let now = jsrt::date::now_ms() as i64;
    let result = if verb == "approve" {
        proposals::approve(&dir, id, &paths.skills_dir(), now, &sha)
            .map(|record| format!("Published {}", record["path"].as_str().unwrap_or_default()))
    } else {
        let reason = (!reason.is_empty()).then(|| reason.join(" "));
        proposals::reject(&dir, id, reason.as_deref(), now).map(|_| format!("Rejected {id}"))
    };
    Some(done(result, out, err))
}

/// The commands that only read (`proposals`, `show`), help and usage.
fn reading(
    args: &[String],
    dir: &std::path::Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Option<Result<i32, String>> {
    let result = match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            return Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io));
        }
        [list] if list == "proposals" => proposals::list(dir).map(|records| match records.is_empty() {
            true => "No skill proposals.".to_string(),
            false => records
                .iter()
                .map(|r| {
                    format!(
                        "{}\t{}\t{}\t{}\t{}",
                        r["id"].as_str().unwrap_or_default(),
                        r["state"].as_str().unwrap_or_default(),
                        r["name"].as_str().unwrap_or_default(),
                        r["segments"].as_array().map_or(0, Vec::len),
                        r["evidence"].as_array().map_or(0, Vec::len),
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }),
        [show, id] if show == "show" => proposals::show(dir, id).map(|(record, content)| shown(&record, &content).0),
        [verb, ..] if matches!(verb.as_str(), "proposals" | "show" | "approve" | "reject") => {
            return Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io));
        }
        _ => return None,
    };
    Some(done(result, out, err))
}
