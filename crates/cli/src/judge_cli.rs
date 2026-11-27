//! `ultron judge bench <cases.json>` (Rust-only): the judge seat's backends measured on labelled cases
//! ([`judge::bench`]): accuracy per kind, latency and spend, under the seat's call ceiling and budget. Every case is a
//! real call to each backend asked for; nothing else here spends. Also the seat the setup wizard probes with.

use crate::Env;
use config::keys::JudgeSettings;
use judge::{DispatchChat, Seat, SeatDeps};
use std::io::Write;
use std::sync::Arc;

const USAGE: &str = "Usage: ultron judge bench <cases.json> [--backend jev|d1|fallback|escalate]... [--limit N] \
[--max-calls N] (see --help)";

pub const HELP: &str = "\
Usage: ultron judge bench <cases.json> [options]

Asks every case of a cases file of each judge backend directly (no fallback, no escalation), interleaved, and
prints accuracy per kind, latency (p50, p95, max), tokens and cost. Each case is one real call per backend.

A cases file is {\"questions\": {<kind>: <question>}, \"cases\": [{\"q\": <kind>, \"gold\": <answer>, \"text\": ...}]}
(an example: crates/judge/tests/data/judgebench.json in the source tree).

Options:
  --backend <name>   jev, d1, fallback (judge.fallbackModel) or escalate (judge.escalateModel); repeat for more.
                     Default: the judge.provider backends (jev, d1).
  --limit <n>        Only the first n cases.
  --max-calls <n>    Stop after n calls in all (default: judge.maxCalls).

Keys: TYPESAFE_API_KEY or auth.json \"judge:typesafe\" (Jev), ULTRON_D1_KEY or auth.json \"judge:d1\" with the judge.d1.url
setting (d1). judge.budgetUsd applies. A backend stops at its first failure.
";

/// The judge seat of `env`'s profile: [`settings`] and [`deps`] over the network.
pub fn seat(env: &Env, chat: bool, max_calls: Option<u64>) -> Result<Seat, String> {
    let mut judge = settings(env);
    if let Some(max_calls) = max_calls {
        judge.max_calls = max_calls;
    }
    Ok(Seat::new(
        judge,
        deps(env, chat, Arc::new(blocking_http::ReqwestTransport::new()?)),
    ))
}

/// The global `judge.*` settings of `env`'s profile.
pub fn settings(env: &Env) -> JudgeSettings {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    config::keys::judge_settings(settings.global_settings())
}

/// What a seat of `env`'s profile is built from: the services' keys, `transport`, and (with `chat`) the fallback and
/// escalation models on the profile's models and keys.
pub fn deps(env: &Env, chat: bool, transport: Arc<dyn blocking_http::Transport>) -> SeatDeps {
    let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
    let mut deps = SeatDeps::with_profile_keys(transport, env.var.clone(), auth);
    if chat {
        let (models, keys) = (env.clone(), env.clone());
        deps.chat = Some(Arc::new(DispatchChat {
            models: Arc::new(move |provider: &str, id: &str| {
                session_core::lane::catalog_model(provider, id, &models).ok()
            }),
            keys: Arc::new(move |provider: &str| {
                session_core::lane::api_key(&session_core::ModelFlags::default(), &keys, provider)
                    .filter(|key| key != ai::env::AMBIENT_CREDENTIALS)
            }),
            transport: Arc::new(ai::ReqwestTransport::for_env(&*env.var, None)),
        }));
        let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
        let global = settings.global_settings();
        let text = |key: &str| global.get(key).and_then(serde_json::Value::as_str);
        deps.session_model = text("defaultProvider")
            .zip(text("defaultModel"))
            .map(|(p, m)| format!("{p}/{m}"));
    }
    deps
}

struct Options {
    file: String,
    backends: Vec<String>,
    limit: Option<usize>,
    max_calls: Option<u64>,
}

fn next_arg(args: &mut std::slice::Iter<'_, String>, name: &str) -> Result<String, String> {
    args.next().cloned().ok_or_else(|| format!("{name} needs a value"))
}

fn parse_positive_count(args: &mut std::slice::Iter<'_, String>, name: &str) -> Result<u64, String> {
    next_arg(args, name)?
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("{name} must be a positive whole number"))
}

fn parse_backend(args: &mut std::slice::Iter<'_, String>) -> Result<String, String> {
    let name = next_arg(args, "--backend")?;
    if !matches!(name.as_str(), "jev" | "d1" | "fallback" | "escalate") {
        return Err(format!("unknown backend {name}"));
    }
    Ok(name)
}

fn apply_bench_flag(options: &mut Options, flag: &str, args: &mut std::slice::Iter<'_, String>) -> Result<(), String> {
    match flag {
        "--backend" => options.backends.push(parse_backend(args)?),
        "--limit" => options.limit = Some(parse_positive_count(args, "--limit")? as usize),
        "--max-calls" => options.max_calls = Some(parse_positive_count(args, "--max-calls")?),
        _ => return Err(format!("unknown option {flag}")),
    }
    Ok(())
}

fn apply_bench_arg(options: &mut Options, arg: &str, args: &mut std::slice::Iter<'_, String>) -> Result<(), String> {
    if arg.starts_with('-') {
        return apply_bench_flag(options, arg, args);
    }
    if options.file.is_empty() {
        options.file = arg.to_string();
        return Ok(());
    }
    Err(format!("unexpected argument {arg}"))
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        file: String::new(),
        backends: Vec::new(),
        limit: None,
        max_calls: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        apply_bench_arg(&mut options, arg, &mut iter)?;
    }
    if options.file.is_empty() {
        return Err("a cases file is required".into());
    }
    Ok(options)
}

fn bench(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    let options = match parse(args) {
        Ok(options) => options,
        Err(message) => {
            writeln!(err, "Error: {message}\n{USAGE}").map_err(io)?;
            return Ok(2);
        }
    };
    let path = env.cwd.join(&options.file);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cases = judge::bench::load(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let chat = options.backends.iter().any(|b| b == "fallback" || b == "escalate");
    let seat = seat(env, chat, options.max_calls)?;
    let backends = if options.backends.is_empty() {
        seat.settings().providers.clone()
    } else {
        options.backends
    };
    let runs = judge::bench::run(&seat, &cases, &backends, options.limit, &kernel::Signal::new());
    for line in judge::bench::report(&runs) {
        writeln!(out, "{line}").map_err(io)?;
    }
    Ok(i32::from(runs.iter().any(|r| r.stopped.is_some())))
}

/// `ultron judge bench ...` and `ultron judge --help`; `None` for anything else (a prompt that starts with "judge").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    let io = |e: std::io::Error| e.to_string();
    match args {
        [help] if matches!(help.as_str(), "--help" | "-h" | "help") => {
            Some(out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io))
        }
        [verb, rest @ ..] if verb == "bench" => Some(if rest.iter().any(|a| a == "--help" || a == "-h") {
            out.write_all(HELP.as_bytes()).map(|()| 0).map_err(io)
        } else {
            bench(rest, env, out, err)
        }),
        _ => None,
    }
}
