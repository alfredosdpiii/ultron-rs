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

fn io(e: std::io::Error) -> String {
    e.to_string()
}

fn write_usage(err: &mut dyn Write, message: &str) -> Result<i32, String> {
    writeln!(err, "Error: {message}").map_err(io)?;
    err.write_all(USAGE.as_bytes()).map_err(io)?;
    Ok(1)
}

fn memory_backend(env: &Env, agent_dir: &Path, project: Option<&Path>) -> Result<Arc<dyn MemoryBackend>, String> {
    let var = |key: &str| (env.var)(key);
    let settings = config::settings::SettingsManager::create(&env.cwd, agent_dir, false);
    let chosen = session_core::compose::memory_backend(&var, &settings, agent_dir);
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
}

struct ImportOptions {
    harness: Harness,
    dry_run: bool,
}

enum ParsedImport {
    Run(ImportOptions),
    Exit(i32),
}

fn parse_import_flags(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Result<ParsedImport, String> {
    let mut from = None;
    let mut dry_run = false;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--from" => from = rest.next().cloned(),
            other if other.starts_with("--from=") => from = Some(other["--from=".len()..].to_string()),
            "--help" | "-h" => {
                out.write_all(USAGE.as_bytes()).map_err(io)?;
                return Ok(ParsedImport::Exit(0));
            }
            other => {
                return Ok(ParsedImport::Exit(write_usage(
                    err,
                    &format!("unknown option: {other}"),
                )?));
            }
        }
    }
    let Some(harness) = from.as_deref().and_then(Harness::parse) else {
        return Ok(ParsedImport::Exit(write_usage(
            err,
            "--from must be claude-code or codex",
        )?));
    };
    Ok(ParsedImport::Run(ImportOptions { harness, dry_run }))
}

fn parse_import_args(argv: &[String], out: &mut dyn Write, err: &mut dyn Write) -> Result<ParsedImport, String> {
    match argv.first().map(String::as_str) {
        Some("import") => parse_import_flags(&argv[1..], out, err),
        Some("--help" | "-h") | None => {
            out.write_all(USAGE.as_bytes()).map_err(io)?;
            Ok(ParsedImport::Exit(i32::from(argv.is_empty())))
        }
        Some(other) => Ok(ParsedImport::Exit(write_usage(
            err,
            &format!("unknown memory command: {other}"),
        )?)),
    }
}

fn action_verb(action: &Action) -> String {
    match action {
        Action::Add => "import".to_string(),
        Action::Replace(id) => format!("replace {id}"),
        Action::Unchanged(id) => format!("unchanged {id}"),
        Action::Refuse(reason) => format!("refuse ({reason})"),
    }
}

fn print_dry_run(
    planned: &[Planned],
    harness: Harness,
    shown: &str,
    home: &Path,
    out: &mut dyn Write,
) -> Result<i32, String> {
    let count = |wanted: fn(&Action) -> bool| planned.iter().filter(|p| wanted(&p.action)).count();
    writeln!(
        out,
        "Dry run: {} notes from {} ({shown}); nothing was written.",
        planned.len(),
        harness.label()
    )
    .map_err(io)?;
    for item in planned {
        writeln!(out, "  {}: {}", action_verb(&item.action), describe(item, home)).map_err(io)?;
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
    Ok(0)
}

#[derive(Default)]
struct ImportCounts {
    imported: usize,
    replaced: usize,
    unchanged: usize,
    refused: usize,
    failed: usize,
}

fn record_outcome(
    item: &Planned,
    outcome: Imported,
    home: &Path,
    counts: &mut ImportCounts,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), String> {
    let shown = describe(item, home);
    let (is_err, line) = match outcome {
        Imported::Unchanged => {
            counts.unchanged += 1;
            return Ok(());
        }
        Imported::Refused(reason) => {
            counts.refused += 1;
            (false, format!("  refused ({reason}): {shown}"))
        }
        Imported::Stored(id) => {
            counts.imported += 1;
            (false, format!("  stored {id}: {shown}"))
        }
        Imported::Replaced(id) => {
            counts.replaced += 1;
            (false, format!("  stored {id}: {shown}"))
        }
        Imported::NotStored(state) => {
            counts.failed += 1;
            (true, format!("  not stored ({state}): {shown}"))
        }
        Imported::Failed(error) => {
            counts.failed += 1;
            (true, format!("  failed ({error}): {shown}"))
        }
    };
    let sink: &mut dyn Write = if is_err { err } else { out };
    writeln!(sink, "{line}").map_err(io)
}

fn execute_import(
    planned: &[Planned],
    harness: Harness,
    agent_dir: &Path,
    env: &Env,
    index: Arc<RecordIndex>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    let journal = agent_dir.join("memory").join("import-journal.json");
    let backend = |project: Option<&Path>| memory_backend(env, agent_dir, project);
    let mut counts = ImportCounts::default();
    memory_import::import_all(planned, &journal, index, &backend, &mut |item, outcome| {
        record_outcome(item, outcome, &env.home, &mut counts, out, err)
    })?;
    writeln!(
        out,
        "Imported {}, replaced {}, unchanged {}, refused {}, failed {} ({} notes from {}).",
        counts.imported,
        counts.replaced,
        counts.unchanged,
        counts.refused,
        counts.failed,
        planned.len(),
        harness.label()
    )
    .map_err(io)?;
    Ok(i32::from(counts.failed > 0))
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    if argv.first().map(String::as_str) == Some("migrate") {
        return crate::memory_migrate::run(&argv[1..], env, out, err);
    }
    let options = match parse_import_args(argv, out, err)? {
        ParsedImport::Run(options) => options,
        ParsedImport::Exit(code) => return Ok(code),
    };
    let root = memory_import::notes_root(options.harness, &env.home, &|key| (env.var)(key));
    let sources = memory_import::discover(options.harness, &root);
    let shown = memory_import::home_relative(&root, &env.home);
    if sources.is_empty() {
        writeln!(
            err,
            "Error: no {} memory notes found in {shown}",
            options.harness.label()
        )
        .map_err(io)?;
        return Ok(1);
    }
    let agent_dir = env.agent_dir();
    let index = Arc::new(RecordIndex::for_agent_dir(&agent_dir));
    let planned = memory_import::plan(&sources, &env.home, &index, &|project| scope_tags(env, project))?;
    if options.dry_run {
        return print_dry_run(&planned, options.harness, &shown, &env.home, out);
    }
    execute_import(&planned, options.harness, &agent_dir, env, index, out, err)
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
