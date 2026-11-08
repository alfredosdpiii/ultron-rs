//! `ultron skills proposals|show|approve|reject` (Rust-only): procedure skills proposed by dreaming or a session
//! (`skills.propose_procedure` in a cell, delegation::procedure::proposals) wait in `<agent dir>/skill-proposals/`
//! until the user publishes one into `<agent dir>/skills/<name>/SKILL.md`. Nothing is published without this
//! command.

use crate::Env;
use delegation::procedure::proposals;
use std::io::Write;

const USAGE: &str = "Usage: ultron skills proposals | show <id> | approve <id> | reject <id> [reason] (see --help)";

pub const HELP: &str = "\
Usage: ultron skills <command>

Procedure skills proposed by dreaming or a session, waiting for your approval.

Commands:
  proposals            List them (tab-separated: ID STATE NAME SEGMENTS EVIDENCE).
  show <id>            Print a proposal's provenance and its SKILL.md.
  approve <id>         Publish it as ~/.ultron/agent/skills/<name>/SKILL.md (never over an existing skill). Edit
                       the proposal's SKILL.md first if you want changes; it is checked again here.
  reject <id> [reason] Keep it out; the proposal stays on record.
";

fn io(e: std::io::Error) -> String {
    e.to_string()
}

/// `ultron skills ...`; `None` when `args` is not a skills command (a prompt that starts with "skills").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    let paths = config::paths::AgentPaths::new(env.agent_dir());
    let dir = paths.skill_proposals();
    let now = jsrt::date::now_ms() as i64;
    let done = |result: Result<String, String>, out: &mut dyn Write, err: &mut dyn Write| match result {
        Ok(text) => writeln!(out, "{text}").map(|()| 0).map_err(io),
        Err(error) => writeln!(err, "Error: {error}").map(|()| 1).map_err(io),
    };
    let result = match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            return Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io));
        }
        [list] if list == "proposals" => proposals::list(&dir).map(|records| match records.is_empty() {
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
        [show, id] if show == "show" => proposals::show(&dir, id).map(|(record, content)| {
            let pretty = serde_json::to_string_pretty(&record).unwrap_or_default();
            format!("{pretty}\n\n{content}")
        }),
        [approve, id] if approve == "approve" => proposals::approve(&dir, id, &paths.skills_dir(), now)
            .map(|record| format!("Published {}", record["path"].as_str().unwrap_or_default())),
        [reject, id, reason @ ..] if reject == "reject" => {
            let reason = (!reason.is_empty()).then(|| reason.join(" "));
            proposals::reject(&dir, id, reason.as_deref(), now).map(|_| format!("Rejected {id}"))
        }
        [verb, ..] if matches!(verb.as_str(), "proposals" | "show" | "approve" | "reject") => {
            return Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io));
        }
        _ => return None,
    };
    Some(done(result, out, err))
}
