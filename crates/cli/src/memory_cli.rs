//! `ultron memory import --from claude-code|codex [--dry-run]`: Claude Code's and Codex's memory notes become typed
//! memory records in the shared memory (kernel_apis::memory_import; memorantado when `memory.backend` chooses it,
//! else Hindsight), each tagged with its source file and checked for secrets as `memory.propose` checks a write.
//! `--dry-run` prints the plan and writes nothing. `ultron memory migrate` is crate::memory_migrate.

use crate::Env;
use kernel_apis::MemoryBackend;
use kernel_apis::memory_import::{self, Action, Harness, Imported, Planned};
use kernel_apis::memory_records::RecordIndex;
use serde_json::{Map, Value};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

pub const USAGE: &str = "Usage: ultron memory import --from claude-code|codex [--dry-run]\n\nImports the memory notes another harness keeps as typed Ultron memory records:\n  claude-code  every ~/.claude/projects/<project>/memory/*.md note (CLAUDE_CONFIG_DIR moves ~/.claude)\n  codex        ~/.codex/memories/memory_summary.md and MEMORY.md (CODEX_HOME moves ~/.codex)\nEach record keeps its source file; a note holding a secret is refused. Running it again replaces changed notes\nand leaves unchanged ones. --dry-run prints what would be imported and writes nothing.\n";

fn scope_tags(env: &Env, project: Option<&Path>) -> Map<String, Value> {
    let cwd = project.unwrap_or(&env.cwd);
    kernel_apis::memory::worker_scope_tags("import", &cwd.display().to_string(), &env.home.display().to_string())
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    if argv.first().map(String::as_str) == Some("migrate") {
        return crate::memory_migrate::run(&argv[1..], env, out, err);
    }
    let agent_dir = env.agent_dir();
    let var = |key: &str| (env.var)(key);
    // The backend sessions use: memorantado once chosen, even when it is down, so an import never lands in Hindsight
    // instead.
    let backend = |project: Option<&Path>| -> Result<Arc<dyn MemoryBackend>, String> {
        let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
        let chosen = session_core::compose::memory_backend(&var, &settings, &agent_dir);
        match chosen.backend {
            memorantado::Backend::Memorantado { endpoint, up: true } => {
                return Ok(Arc::new(
                    endpoint.backend(scope_tags(env, project)).map_err(|e| e.to_string())?,
                ));
            }
            memorantado::Backend::Memorantado { endpoint, up: false } => return Err(endpoint.paused_notice()),
            memorantado::Backend::Off if chosen.hindsight.is_some() => {
                return Err("memorantado, the memory backend, is turned off (ULTRON_MEMORANTADO_URL).".into());
            }
            memorantado::Backend::Off | memorantado::Backend::Hindsight => {}
        }
        let endpoint = chosen
            .hindsight
            .ok_or("Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.")?;
        let mut options = hindsight::HindsightOptions::new(&endpoint.url, &endpoint.bank, scope_tags(env, project));
        options.ensure_bank = true;
        Ok(Arc::new(
            hindsight::HindsightBackend::new(options).map_err(|e| e.to_string())?,
        ))
    };
    let io = |e: std::io::Error| e.to_string();
    let usage = |err: &mut dyn Write, message: &str| -> Result<i32, String> {
        writeln!(err, "Error: {message}").map_err(io)?;
        err.write_all(USAGE.as_bytes()).map_err(io)?;
        Ok(1)
    };
    match argv.first().map(String::as_str) {
        Some("import") => {}
        Some("--help" | "-h") | None => {
            out.write_all(USAGE.as_bytes()).map_err(io)?;
            return Ok(if argv.is_empty() { 1 } else { 0 });
        }
        Some(other) => return usage(err, &format!("unknown memory command: {other}")),
    }
    let mut from = None;
    let mut dry_run = false;
    let mut rest = argv[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--from" => from = rest.next().cloned(),
            other if other.starts_with("--from=") => from = Some(other["--from=".len()..].to_string()),
            "--help" | "-h" => {
                out.write_all(USAGE.as_bytes()).map_err(io)?;
                return Ok(0);
            }
            other => return usage(err, &format!("unknown option: {other}")),
        }
    }
    let Some(harness) = from.as_deref().and_then(Harness::parse) else {
        return usage(err, "--from must be claude-code or codex");
    };
    let root = memory_import::notes_root(harness, &env.home, &|key| (env.var)(key));
    let sources = memory_import::discover(harness, &root);
    let shown = memory_import::home_relative(&root, &env.home);
    if sources.is_empty() {
        writeln!(err, "Error: no {} memory notes found in {shown}", harness.label()).map_err(io)?;
        return Ok(1);
    }
    let agent_dir = env.agent_dir();
    let index = Arc::new(RecordIndex::for_agent_dir(&agent_dir));
    let planned = memory_import::plan(&sources, &env.home, &index, &|project| scope_tags(env, project))?;
    let count = |wanted: fn(&Action) -> bool| planned.iter().filter(|p| wanted(&p.action)).count();
    if dry_run {
        writeln!(
            out,
            "Dry run: {} notes from {} ({shown}); nothing was written.",
            planned.len(),
            harness.label()
        )
        .map_err(io)?;
        for item in &planned {
            let verb = match &item.action {
                Action::Add => "import".to_string(),
                Action::Replace(id) => format!("replace {id}"),
                Action::Unchanged(id) => format!("unchanged {id}"),
                Action::Refuse(reason) => format!("refuse ({reason})"),
            };
            writeln!(out, "  {verb}: {}", describe(item, &env.home)).map_err(io)?;
        }
        writeln!(
            out,
            "{} to import, {} to replace, {} unchanged, {} refused.",
            count(|a| matches!(a, Action::Add)),
            count(|a| matches!(a, Action::Replace(_))),
            count(|a| matches!(a, Action::Unchanged(_))),
            count(|a| matches!(a, Action::Refuse(_)))
        )
        .map_err(io)?;
        return Ok(0);
    }
    let journal = agent_dir.join("memory").join("import-journal.json");
    let (mut imported, mut replaced, mut unchanged, mut refused, mut failed) = (0, 0, 0, 0, 0);
    memory_import::import_all(&planned, &journal, index, &backend, &mut |item, outcome| {
        let shown = describe(item, &env.home);
        match outcome {
            Imported::Unchanged => unchanged += 1,
            Imported::Refused(reason) => {
                refused += 1;
                writeln!(out, "  refused ({reason}): {shown}").map_err(io)?;
            }
            Imported::Stored(id) => {
                imported += 1;
                writeln!(out, "  stored {id}: {shown}").map_err(io)?;
            }
            Imported::Replaced(id) => {
                replaced += 1;
                writeln!(out, "  stored {id}: {shown}").map_err(io)?;
            }
            Imported::NotStored(state) => {
                failed += 1;
                writeln!(err, "  not stored ({state}): {shown}").map_err(io)?;
            }
            Imported::Failed(error) => {
                failed += 1;
                writeln!(err, "  failed ({error}): {shown}").map_err(io)?;
            }
        }
        Ok(())
    })?;
    writeln!(
        out,
        "Imported {imported}, replaced {replaced}, unchanged {unchanged}, refused {refused}, failed {failed} ({} notes from {}).",
        planned.len(),
        harness.label()
    )
    .map_err(io)?;
    Ok(if failed > 0 { 1 } else { 0 })
}

/// `<type> <key> (<scope>[ <project>]) from <source>`.
fn describe(item: &Planned, home: &Path) -> String {
    let scope = match &item.project {
        Some(project) => format!("project {}", memory_import::home_relative(project, home)),
        None => item.scope.to_string(),
    };
    format!(
        "{} {} ({scope}) from {}",
        item.typed.kind,
        item.typed.key.as_deref().unwrap_or(""),
        item.typed.source.as_deref().unwrap_or("")
    )
}
