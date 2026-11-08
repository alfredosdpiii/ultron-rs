//! `ultron optimize <judge|memory|autoreview>` (Rust-only): variants of one of Ultron's own pipelines measured on its
//! frozen test set ([`optimize`]), each a versioned cost/quality point kept under `<agent dir>/optimize/`. Offline by
//! default; `--live` (with `--budget`) is the only way it spends.

use crate::Env;
use optimize::judge::{JudgeSet, Recorder, Recording, Replay};
use optimize::memory::{MemorySet, Reference};
use optimize::{Point, Store, frontier};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const USAGE: &str = "Usage: ultron optimize <judge|memory|autoreview> [options] (see --help)";

pub const HELP: &str = "\
Usage: ultron optimize <pipeline> [options]

Runs variants of one of Ultron's pipelines on its frozen test set and prints one point per variant: the failure
surface's outcome (passed, failed, incomplete), quality, a penalty, cost and failures; * marks the Pareto frontier.
Points and versioned variants are kept under <agent dir>/optimize/<pipeline>/. Nothing here changes a setting.

Pipelines:
  judge        The judge seat on the judgebench cases, answered from a frozen recording of a live run (Jev, d1).
               Fits judge.thresholds per question kind and backend and writes them as a proposal file.
               Variants set provider, thresholds, model, escalateModel, fallbackModel (as the judge setting).
  memory       Scoped recall on memorantado's bench:ultron set: recall@5, MRR, scope and retired leakage.
               Offline on an in-process reference backend. Variants set backend (reference, hindsight), limit,
               minScore.
  autoreview   Reports of `ultron autoreview eval` re-scored at each blockAt level (critical .. nit).
               Arguments: report files; default the newest case set under <agent dir>/autoreview/evals/.

Options:
  --variants <file>   [{\"name\": ..., \"config\": {...}}] instead of the built-in variants (judge, memory).
  --recording <file>  judge: replay this recording instead of the frozen one.
  --live              judge: ask Jev and d1 every case once and replay the new recording (saved under recordings/);
                      memory: run hindsight variants on a throwaway bank of the configured server.
  --budget <usd>      Required with --live: the judge's budget (judge.budgetUsd for the run).
  --max-calls <n>     judge --live: the call ceiling (default: cases x backends).
  --json              Print the point records as JSON.
";

#[derive(Default)]
struct Options {
    pipeline: String,
    files: Vec<String>,
    variants: Option<String>,
    recording: Option<String>,
    live: bool,
    budget: Option<f64>,
    max_calls: Option<u64>,
    json: bool,
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--variants" => options.variants = Some(value("--variants")?),
            "--recording" => options.recording = Some(value("--recording")?),
            "--live" => options.live = true,
            "--json" => options.json = true,
            "--budget" => {
                let budget = value("--budget")?;
                options.budget = Some(
                    budget
                        .parse::<f64>()
                        .ok()
                        .filter(|b| b.is_finite() && *b > 0.0)
                        .ok_or("--budget must be a positive number of USD")?,
                );
            }
            "--max-calls" => {
                let calls = value("--max-calls")?;
                options.max_calls = Some(
                    calls
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or("--max-calls must be a positive whole number")?,
                );
            }
            flag if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            word if options.pipeline.is_empty() => options.pipeline = word.to_string(),
            file => options.files.push(file.to_string()),
        }
    }
    let pipeline = options.pipeline.as_str();
    if options.live && options.budget.is_none() {
        return Err("--live needs --budget <usd>".into());
    }
    if !options.live && (options.budget.is_some() || options.max_calls.is_some()) {
        return Err("--budget and --max-calls apply to --live runs".into());
    }
    if options.live && pipeline == "autoreview" {
        return Err("autoreview has no --live: run `ultron autoreview eval <repo>` and pass its report".into());
    }
    if !options.files.is_empty() && pipeline != "autoreview" {
        return Err(format!("unexpected argument {}", options.files[0]));
    }
    if options.recording.is_some() && (pipeline != "judge" || options.live) {
        return Err("--recording applies to offline judge runs".into());
    }
    if options.variants.is_some() && pipeline == "autoreview" {
        return Err("autoreview variants are its reports and levels; --variants does not apply".into());
    }
    Ok(options)
}

/// A variants file: `[{name, config}]`.
fn read_variants(path: &Path) -> Result<Vec<(String, Value)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let list: Value = serde_json::from_str(&text).map_err(|e| format!("{}: not JSON: {e}", path.display()))?;
    list.as_array()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| format!("{}: must be a nonempty list of {{name, config}}", path.display()))?
        .iter()
        .enumerate()
        .map(|(i, v)| match (v["name"].as_str(), v["config"].is_object()) {
            (Some(name), true) => Ok((name.to_string(), v["config"].clone())),
            _ => Err(format!("{}: [{i}] needs name and config", path.display())),
        })
        .collect()
}

/// A measured variant before it is versioned.
type Measured = (String, Value, Point);

struct Run<'a> {
    options: &'a Options,
    env: &'a Env,
    store: &'a Store,
    at: String,
    stamp: String,
    variants: Option<Vec<(String, Value)>>,
    err: &'a mut dyn Write,
}

fn judge(run: &mut Run) -> Result<Vec<Measured>, String> {
    let set = Arc::new(JudgeSet::load(optimize::judge::CASES)?);
    let signal = kernel::Signal::new();
    let (recording, mode) = if run.options.live {
        let backends: Vec<String> = match &run.variants {
            Some(variants) => {
                let mut names: Vec<String> = Vec::new();
                for (_, config) in variants {
                    for name in config["provider"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !names.iter().any(|n| n == name) {
                            names.push(name.to_string());
                        }
                    }
                }
                names
            }
            None => config::keys::JUDGE_BACKENDS.iter().map(|b| b.to_string()).collect(),
        };
        let mut settings = crate::judge_cli::settings(run.env);
        settings.budget_usd = run.options.budget;
        settings.max_calls = run
            .options
            .max_calls
            .unwrap_or((set.bench.cases.len() * backends.len()) as u64);
        let recorder = Arc::new(Recorder::new(
            Arc::new(blocking_http::ReqwestTransport::new()?),
            set.clone(),
        ));
        let seat = judge::Seat::new(settings, crate::judge_cli::deps(run.env, false, recorder.clone()));
        let runs = judge::bench::run(&seat, &set.bench, &backends, None, &signal);
        for line in judge::bench::report(&runs) {
            let _ = writeln!(run.err, "{line}");
        }
        let recording = recorder.recording(format!("ultron optimize judge --live, {}", run.at));
        let path = run
            .store
            .write("judge", "recordings", &run.stamp, &recording.to_json())?;
        let _ = writeln!(run.err, "Recording: {}", path.display());
        (recording, "live")
    } else {
        let text = match &run.options.recording {
            Some(file) => {
                let path = run.env.cwd.join(file);
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
            }
            None => optimize::judge::RECORDING.to_string(),
        };
        (Recording::parse(&text)?, "offline")
    };
    let replay = Arc::new(Replay::new(set.clone(), recording.clone())?);
    let none: judge::backend::KeySource = Arc::new(|_: &str| Ok(None));
    let current = judge::Seat::new(
        crate::judge_cli::settings(run.env),
        judge::SeatDeps::new(replay.clone(), none.clone(), none),
    );
    let fits = optimize::judge::fit(&set, &recording, &|kind, backend| {
        current.threshold(Some(kind), backend)
    });
    for f in &fits {
        let shown = |t: Option<f64>| t.map_or_else(|| "none".to_string(), |t| format!("{t:.2}"));
        let _ = writeln!(
            run.err,
            "fit {} {}: {} answers, {} wrong; threshold {} -> {} ({} -> {} unsure): {}",
            f.kind,
            f.backend,
            f.answers,
            f.wrong,
            shown(f.current),
            shown(f.fitted),
            f.unsure_now,
            f.unsure_fitted,
            f.note
        );
    }
    let proposal = optimize::judge::proposal(&set, &recording, &fits, &run.at);
    let path = run.store.write("judge", "proposals", &run.stamp, &proposal)?;
    let _ = writeln!(run.err, "Threshold proposal (not applied): {}", path.display());
    let variants = run
        .variants
        .take()
        .unwrap_or_else(|| optimize::judge::default_variants(&fits));
    let mut measured = Vec::new();
    for (name, config) in variants {
        optimize::judge::check_config(&config).map_err(|e| format!("variant {name}: {e}"))?;
        replay.take_ms();
        let seat = optimize::judge::replay_seat(&config, replay.clone());
        let verdicts = optimize::judge::decide_all(&seat, &set, &signal);
        let mut point = optimize::judge::point(&set, &config, &verdicts, &replay.take_ms());
        point.mode = mode;
        measured.push((name, config, point));
    }
    Ok(measured)
}

fn memory(run: &mut Run) -> Result<Vec<Measured>, String> {
    let set = MemorySet::load(optimize::memory::SET)?;
    let signal = kernel::Signal::new();
    let variants = run.variants.take().unwrap_or_else(|| {
        let mut variants = vec![
            ("reference".to_string(), json!({"backend": "reference"})),
            (
                "reference-min0.5".to_string(),
                json!({"backend": "reference", "minScore": 0.5}),
            ),
        ];
        if run.options.live {
            variants.push(("hindsight".to_string(), json!({"backend": "hindsight"})));
        }
        variants
    });
    let mut measured = Vec::new();
    for (name, config) in variants {
        optimize::memory::check_config(&config).map_err(|e| format!("variant {name}: {e}"))?;
        let live = config["backend"] != "reference";
        let backend: Arc<dyn kernel_apis::MemoryBackend> = if live {
            if !run.options.live {
                return Err(format!(
                    "variant {name}: the {} backend runs only with --live",
                    config["backend"]
                ));
            }
            let agent_dir = run.env.agent_dir();
            let endpoint = hindsight::Endpoint::from_env(&|key| (run.env.var)(key), &run.env.cwd, &agent_dir)
                .ok_or("Hindsight is not configured (hindsightUrl, ULTRON_HINDSIGHT_URL)")?;
            let bank = format!("ultron-optimize-{}", run.stamp);
            let mut options = hindsight::HindsightOptions::new(&endpoint.url, &bank, serde_json::Map::new());
            options.ensure_bank = true;
            let _ = writeln!(
                run.err,
                "{name}: throwaway bank {bank} on {} (left in place; Hindsight's own model spend is not metered here)",
                endpoint.url
            );
            Arc::new(hindsight::HindsightBackend::new(options).map_err(|e| e.to_string())?)
        } else {
            Arc::new(Reference::default())
        };
        optimize::memory::load(&*backend, &set, &signal).map_err(|e| format!("variant {name}: {e}"))?;
        let mut point = optimize::memory::measure(&*backend, &set, &config, &signal);
        if live {
            point.mode = "live";
        } else {
            // The reference backend runs in process: it costs nothing.
            point.cost_usd = Some(0.0);
        }
        measured.push((name, config, point));
    }
    Ok(measured)
}

/// A variant name from free text (a model reference): lowercase, other characters as `-`.
fn slug(text: &str) -> String {
    let slug: String = text
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').chars().take(48).collect::<String>();
    if slug.is_empty() { "report".into() } else { slug }
}

fn autoreview(run: &mut Run) -> Result<Vec<Measured>, String> {
    let files: Vec<PathBuf> = if run.options.files.is_empty() {
        let dir = run.env.agent_dir().join("autoreview").join("evals");
        let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|_| {
                format!(
                    "no reports under {} (run `ultron autoreview eval <repo>` first)",
                    dir.display()
                )
            })?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        found.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
        found
    } else {
        run.options.files.iter().map(|f| run.env.cwd.join(f)).collect()
    };
    let mut reports = Vec::new();
    for path in &files {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let stem = path
            .file_stem()
            .map_or("report".into(), |s| s.to_string_lossy().into_owned());
        let report =
            optimize::autoreview::parse_report(&text, &stem).map_err(|e| format!("{}: {e}", path.display()))?;
        reports.push((path.clone(), report));
    }
    // Without named reports: those of the newest report's case set.
    if run.options.files.is_empty() {
        let newest = reports.last().map(|(_, r)| r.hash.clone()).ok_or("no reports found")?;
        reports.retain(|(_, r)| r.hash == newest);
    }
    let mut measured = Vec::new();
    for (path, report) in &reports {
        for level in review::plan::LEVELS {
            let name = format!("{}.{level}", slug(&report.name));
            let config = json!({"report": path.display().to_string(), "model": report.name, "blockAt": level});
            measured.push((name, config, optimize::autoreview::point(report, level)));
        }
    }
    Ok(measured)
}

fn optimize(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            writeln!(err, "Error: {message}\n{USAGE}").map_err(io)?;
            return Ok(2);
        }
    };
    let variants = match &options.variants {
        Some(file) => Some(read_variants(&env.cwd.join(file))?),
        None => None,
    };
    let store = Store::new(config::paths::AgentPaths::new(env.agent_dir()).optimize());
    let now = jsrt::date::now_ms();
    let mut run = Run {
        options: &options,
        env,
        store: &store,
        at: jsrt::date::iso_string(now),
        stamp: format!("{now}"),
        variants,
        err,
    };
    let measured = match options.pipeline.as_str() {
        "judge" => judge(&mut run)?,
        "memory" => memory(&mut run)?,
        _ => autoreview(&mut run)?,
    };
    let at = run.at.clone();
    let mut points = Vec::new();
    for (name, config, mut point) in measured {
        point.variant = name;
        point.version = store.variant(point.pipeline, &point.variant, &config, &at)?;
        store.record(&point, &at)?;
        points.push(point);
    }
    let on = frontier(&points);
    if options.json {
        let records: Vec<Value> = points
            .iter()
            .zip(&on)
            .map(|(p, on)| {
                let mut record = p.to_json(&at);
                record["frontier"] = json!(on);
                record
            })
            .collect();
        writeln!(out, "{}", json!({ "points": records })).map_err(io)?;
    } else {
        for (point, on) in points.iter().zip(&on) {
            writeln!(out, "{}", point.line(*on)).map_err(io)?;
        }
        writeln!(
            out,
            "Kept in {}",
            config::paths::AgentPaths::new(env.agent_dir())
                .optimize()
                .join(&options.pipeline)
                .join("points.jsonl")
                .display()
        )
        .map_err(io)?;
    }
    Ok(0)
}

/// `ultron optimize <pipeline> ...` and `ultron optimize --help`; `None` for anything else (a prompt that starts
/// with "optimize").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    let io = |e: std::io::Error| e.to_string();
    match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io))
        }
        [pipeline, rest @ ..] if matches!(pipeline.as_str(), "judge" | "memory" | "autoreview") => {
            Some(if rest.iter().any(|a| a == "--help" || a == "-h") {
                out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io)
            } else {
                optimize(args, env, out, err)
            })
        }
        _ => None,
    }
}
