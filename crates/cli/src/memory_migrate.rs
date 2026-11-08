//! `ultron memory migrate --to memorantado [--dry-run]`: a one-time copy of the shared Hindsight bank into
//! memorantado. Every Hindsight document keeps its id as its memorantado key, its tags (so its scope) and its
//! metadata (type, situation, quotes, evidence class); a session's summaries document (appended to over time) becomes
//! an episode. Run `--dry-run` first: it lists what would be copied and writes nothing. A copied document is noted
//! in the agent directory's migrate journal with its text's hash, so running it again copies only what changed.

use crate::Env;
use kernel::Signal;
use kernel_apis::MemoryBackend;
use serde_json::{Map, Value, json};
use std::io::Write;
use std::path::Path;

pub const USAGE: &str = "Usage: ultron memory migrate --to memorantado [--dry-run]\n\nCopies the shared Hindsight memory into memorantado once: each document keeps its id (as the claim's key), tags\nand metadata; a session's summaries become an episode. Run --dry-run first: it lists what would be copied and\nwrites nothing. Running it again copies only what changed since.\n";

/// Documents read per Hindsight page.
const PAGE: u32 = 100;

/// What a Hindsight document becomes.
#[derive(Debug, PartialEq)]
enum Plan {
    Claim,
    Episode,
    Unchanged,
    Skip(&'static str),
}

fn plan(document: &Value, journal: &Map<String, Value>) -> Plan {
    let id = document["id"].as_str().unwrap_or_default();
    let text = document["original_text"].as_str().unwrap_or_default();
    if jsrt::string::trim(text).is_empty() {
        return Plan::Skip("no text");
    }
    if document["tags"].as_array().is_none_or(Vec::is_empty) {
        return Plan::Skip("no scope tags");
    }
    if journal.get(id).and_then(Value::as_str) == Some(jsrt::hash::sha256_hex(text.as_bytes()).as_str()) {
        return Plan::Unchanged;
    }
    if id.ends_with(":summaries") {
        Plan::Episode
    } else {
        Plan::Claim
    }
}

fn read_journal(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

fn write_journal(path: &Path, journal: &Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, Value::Object(journal.clone()).to_string()).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let mut to = None;
    let mut dry_run = false;
    let mut rest = argv.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--to" => to = rest.next().cloned(),
            other if other.starts_with("--to=") => to = Some(other["--to=".len()..].to_string()),
            "--help" | "-h" => {
                out.write_all(USAGE.as_bytes()).map_err(io)?;
                return Ok(0);
            }
            other => {
                writeln!(err, "Error: unknown option: {other}").map_err(io)?;
                err.write_all(USAGE.as_bytes()).map_err(io)?;
                return Ok(1);
            }
        }
    }
    if to.as_deref() != Some("memorantado") {
        writeln!(err, "Error: --to must be memorantado").map_err(io)?;
        err.write_all(USAGE.as_bytes()).map_err(io)?;
        return Ok(1);
    }
    let agent_dir = env.agent_dir();
    let var = |key: &str| (env.var)(key);
    let settings = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
    let Some(source) = hindsight::Endpoint::from_env(&var, &env.cwd, &agent_dir) else {
        writeln!(err, "Error: Hindsight is not configured. Set ULTRON_HINDSIGHT_URL.").map_err(io)?;
        return Ok(1);
    };
    let Some(target) = memorantado::Endpoint::configured(&var, &settings) else {
        writeln!(err, "Error: memorantado is turned off (ULTRON_MEMORANTADO_URL).").map_err(io)?;
        return Ok(1);
    };
    let signal = Signal::new();
    let hindsight =
        hindsight::HindsightBackend::new(hindsight::HindsightOptions::new(&source.url, &source.bank, Map::new()))
            .map_err(|e| e.to_string())?;
    let journal_path = agent_dir.join("memory").join("migrate-journal.json");
    let mut journal = read_journal(&journal_path);
    let mut documents = Vec::new();
    let mut offset = 0;
    loop {
        let page = hindsight
            .documents(PAGE, offset, &signal)
            .map_err(|e| format!("Hindsight at {}: {e}", source.url))?;
        let items = page["items"].as_array().cloned().unwrap_or_default();
        for item in &items {
            let Some(id) = item["id"].as_str() else { continue };
            documents.push(
                hindsight
                    .get(id, &signal)
                    .map_err(|e| format!("Hindsight document {id}: {e}"))?,
            );
        }
        offset += items.len() as u32;
        if items.is_empty() || page["total"].as_u64().is_some_and(|total| u64::from(offset) >= total) {
            break;
        }
    }
    let planned: Vec<(Value, Plan)> = documents
        .into_iter()
        .map(|d| {
            let plan = plan(&d, &journal);
            (d, plan)
        })
        .collect();
    let count = |wanted: fn(&Plan) -> bool| planned.iter().filter(|(_, p)| wanted(p)).count();
    let summary = format!(
        "{} claims, {} episodes, {} unchanged, {} skipped",
        count(|p| *p == Plan::Claim),
        count(|p| *p == Plan::Episode),
        count(|p| *p == Plan::Unchanged),
        count(|p| matches!(p, Plan::Skip(_)))
    );
    if dry_run {
        writeln!(
            out,
            "Dry run: {} Hindsight documents (bank {} at {}) to memorantado project {} at {}; nothing was written.",
            planned.len(),
            source.bank,
            source.url,
            target.project,
            target.url
        )
        .map_err(io)?;
        for (document, plan) in &planned {
            let verb = match plan {
                Plan::Claim => "claim".to_string(),
                Plan::Episode => "episode".to_string(),
                Plan::Unchanged => "unchanged".to_string(),
                Plan::Skip(why) => format!("skip ({why})"),
            };
            writeln!(out, "  {verb}: {}", document["id"].as_str().unwrap_or_default()).map_err(io)?;
        }
        writeln!(out, "{summary}.").map_err(io)?;
        return Ok(0);
    }
    let transport = memorantado::ReqwestTransport::new()?;
    if !memorantado::resolve::answers(&transport, &target.url) {
        writeln!(err, "Error: memorantado does not answer at {}.", target.url).map_err(io)?;
        return Ok(1);
    }
    let memorantado = target.backend(Map::new()).map_err(|e| e.to_string())?;
    let (mut copied, mut failed) = (0, 0);
    for (document, plan) in &planned {
        let mode = match plan {
            Plan::Claim => "replace",
            Plan::Episode => "append",
            _ => continue,
        };
        let id = document["id"].as_str().unwrap_or_default();
        let text = document["original_text"].as_str().unwrap_or_default();
        let metadata = if document["document_metadata"].is_object() {
            document["document_metadata"].clone()
        } else {
            json!({})
        };
        let item = json!({"content": text, "document_id": id, "tags": document["tags"], "update_mode": mode,
            "metadata": metadata});
        match memorantado.retain(&json!({"items": [item]}), &signal) {
            Ok(_) => {
                copied += 1;
                journal.insert(id.to_string(), jsrt::hash::sha256_hex(text.as_bytes()).into());
            }
            Err(error) => {
                failed += 1;
                writeln!(err, "  failed ({error}): {id}").map_err(io)?;
            }
        }
    }
    write_journal(&journal_path, &journal)?;
    writeln!(out, "Copied {copied}, failed {failed} ({summary}).").map_err(io)?;
    Ok(if failed > 0 { 1 } else { 0 })
}
