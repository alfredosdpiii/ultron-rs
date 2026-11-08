//! `ultron migrate ...` (`ultron/migration-cli.ts`, `ultron/migration.ts`): profile backup and restore, and the
//! command line of Pi <-> Ultron session migration. Output names paths and counts only; credential file contents are
//! never printed.
//! `import-pi` and `export-pi` run `session_core::migration`.

use crate::Env;
use std::path::{Path, PathBuf};

fn usage() -> String {
    [
        "Usage: ultron migrate <command>",
        "",
        "Commands:",
        "  import-pi <pi-session.jsonl> [--sessions-root <dir>]   Import a whole Pi session tree as a native session",
        "  export-pi <native-session.jsonl> <out.jsonl>          Write a native session tree as a Pi session",
        "  backup [dir] [--agent-dir <dir>]                      Back up the config profile (not sessions)",
        "  restore <backup-dir> [--agent-dir <dir>]              Verify and restore a profile backup",
    ]
    .join("\n")
}

enum Failure {
    Usage(String),
    Error(String),
}

struct Options {
    positional: Vec<String>,
    options: Vec<(String, String)>,
}

impl Options {
    fn get(&self, name: &str) -> Option<&str> {
        self.options
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn parse_options(args: &[String], flags: &[&str]) -> Result<Options, Failure> {
    let mut parsed = Options {
        positional: Vec::new(),
        options: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(body) = arg.strip_prefix("--") {
            // `split("=", 2)`: the inline value stops at a second `=`.
            let mut parts = body.split('=');
            let name = parts.next().unwrap_or_default().to_string();
            let inline = parts.next().map(str::to_string);
            if !flags.contains(&name.as_str()) {
                return Err(Failure::Usage(format!("Unknown option --{name}")));
            }
            let value = match inline {
                Some(v) => Some(v),
                None => {
                    index += 1;
                    args.get(index).cloned()
                }
            };
            match value {
                Some(v) if !v.is_empty() => parsed.options.push((name, v)),
                _ => return Err(Failure::Usage(format!("Option --{name} requires a value"))),
            }
        } else {
            parsed.positional.push(arg.clone());
        }
        index += 1;
    }
    Ok(parsed)
}

fn expect_positional(positional: &[String], min: usize, max: usize) -> Result<(), Failure> {
    if positional.len() < min {
        return Err(Failure::Usage("Missing argument".into()));
    }
    if positional.len() > max {
        return Err(Failure::Usage(format!("Unexpected argument: {}", positional[max])));
    }
    Ok(())
}

/// Handle `migrate ...`; None when `argv` is not a migrate command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("migrate") {
        return None;
    }
    let sub = argv.get(1).map(String::as_str);
    if matches!(sub, None | Some("--help" | "-h" | "help")) {
        let _ = writeln!(out, "{}", usage());
        return Some(i32::from(sub.is_none()));
    }
    let rest = &argv[2..];
    match command(sub.unwrap_or_default(), rest, env) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            Some(0)
        }
        Err(Failure::Usage(message)) => {
            let _ = writeln!(err, "Error: {message}\n{}", usage());
            Some(1)
        }
        Err(Failure::Error(message)) => {
            let _ = writeln!(err, "Error: {message}");
            Some(1)
        }
    }
}

fn resolve(env: &Env, path: &str) -> PathBuf {
    config::paths::normalize(&env.cwd.join(path))
}

fn command(sub: &str, rest: &[String], env: &Env) -> Result<Vec<String>, Failure> {
    let agent_dir = |o: &Options| o.get("agent-dir").map_or_else(|| env.agent_dir(), PathBuf::from);
    match sub {
        "import-pi" => {
            let o = parse_options(rest, &["sessions-root"])?;
            expect_positional(&o.positional, 1, 1)?;
            let root_input = o
                .get("sessions-root")
                .map(str::to_string)
                .or_else(|| (env.var)(config::paths::ENV_SESSION_DIR))
                .unwrap_or_else(|| {
                    env.agent_dir()
                        .join("experimental")
                        .join("sessions")
                        .display()
                        .to_string()
                });
            let sessions_root = config::paths::resolve(&root_input, &env.cwd, &env.home);
            let result = session_core::migration::import_pi_session(&resolve(env, &o.positional[0]), &sessions_root)
                .map_err(|e| Failure::Error(e.to_string()))?;
            Ok(import_lines(&result))
        }
        "export-pi" => {
            let o = parse_options(rest, &[])?;
            expect_positional(&o.positional, 2, 2)?;
            let result = session_core::migration::export_native_session_to_pi(
                &resolve(env, &o.positional[0]),
                &resolve(env, &o.positional[1]),
            )
            .map_err(Failure::Error)?;
            Ok(vec![format!(
                "Exported native session {} ({} entries) to {}",
                result.session_id,
                result.entries,
                result.path.display()
            )])
        }
        "backup" => {
            let o = parse_options(rest, &["agent-dir"])?;
            expect_positional(&o.positional, 0, 1)?;
            let agent = agent_dir(&o);
            let backup_dir = match o.positional.first() {
                Some(dir) => PathBuf::from(dir),
                None => agent.parent().unwrap_or(Path::new("/")).join("backups"),
            };
            let (path, count) = config::profile_backup::backup(
                &resolve(env, &agent.to_string_lossy()),
                &resolve(env, &backup_dir.to_string_lossy()),
            )
            .map_err(Failure::Error)?;
            Ok(vec![format!("Backed up {count} profile entries to {}", path.display())])
        }
        "restore" => {
            let o = parse_options(rest, &["agent-dir"])?;
            expect_positional(&o.positional, 1, 1)?;
            let agent = agent_dir(&o);
            let restored = config::profile_backup::restore(
                &resolve(env, &o.positional[0]),
                &resolve(env, &agent.to_string_lossy()),
            )
            .map_err(Failure::Error)?;
            Ok(vec![format!(
                "Restored {restored} profile entries to {}",
                agent.display()
            )])
        }
        other => Err(Failure::Usage(format!("Unknown migrate command: {other}"))),
    }
}

/// What `migrate import-pi` prints for an import: paths and counts only.
fn import_lines(result: &session_core::migration::ImportResult) -> Vec<String> {
    let entries = |count: usize| if count == 1 { "entry" } else { "entries" };
    let mut lines = vec![
        format!(
            "Imported {} entries as native session {}",
            result.imported, result.session_id
        ),
        format!("  {}", result.path.display()),
    ];
    for (kind, count) in &result.skipped {
        lines.push(format!("  skipped {count} {kind} {}", entries(*count)));
    }
    for (kind, count) in &result.preserved {
        lines.push(format!(
            "  preserved {count} unknown {kind} {} as custom entries",
            entries(*count)
        ));
    }
    if result.branch_copies > 0 {
        lines.push(format!(
            "  applied branch-local context edits per branch with {} copied {}",
            result.branch_copies,
            entries(result.branch_copies)
        ));
    }
    for id in &result.unapplied_context_edits {
        lines.push(format!(
            "  context edit {id} does not target a message on its own path; kept as an entry, not applied"
        ));
    }
    if result.usage_entries > 0 {
        lines.push(format!(
            "  imported usage of {} Pi {} (${}) into usage root {}",
            result.usage_entries,
            entries(result.usage_entries),
            jsrt::number::to_string(result.usage_cost),
            result.usage_root
        ));
    }
    lines
}
