//! `ultron autoreview` (`autoreview/cli.ts`): the automated pull-request reviewer's command.
//!
//!   ultron autoreview review --repo-dir <dir> --base <sha> --head <sha> [--json] [--dry-run]
//!                                               review a local diff, no GitHub access (the benchmark entry)
//!   ultron autoreview review <owner/repo#N|URL> review one pull request now
//!   ultron autoreview eval <repo-dir>          the reviewer measured on the repository's reverted and fixed commits
//!   ultron autoreview run | once | status | doctor | prepare | install | uninstall
//!
//! RLM-first: a review is `autoreview_api.run` in a Python kernel of its own (`review::engine`); its only model calls
//! are `rlm.map` frames on the models the configuration names. stdout carries only the command's own output (one
//! JSON object with `--json`); log lines go to stderr.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use review::engine::{EngineOptions, KernelEngine, Log, ModelResolver, ReviewEngine};
use review::plan::{EngineResult, PlanOptions, decide_verdict, level_of, plan_review, rank_findings, severity_of};
use review::settings::{
    AutoreviewConfig, AutoreviewSettings, MIN_BUDGET_TOKENS, ModelFallbacks, RECOMMENDED_MODELS, autoreview_paths,
    engine_settings, resolve_config,
};
use serde_json::{Map, Value, json};
use session_core::ModelFlags;

use crate::Env;

mod commands;
mod offline;

pub use commands::{Overrides, age, doctor_prepared_lines, outcome_json, outcome_text, user_service};
pub use offline::{describe_models, describe_stages, offline_json};

use commands::{
    ban_command, daemon_command, doctor, model_lines, prepare_command, prepare_vars, review_pull_request, run_prepare,
    service, status,
};
use offline::{eval_command, index_knowledge, labels_command, offline_spec, register_command};

pub(super) const APP_NAME: &str = "ultron";
pub(super) const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
pub(super) const REVIEW_MODES: [&str; 7] = ["fast", "deep", "both", "compiled", "hybrid", "auto", "minimal"];
pub(super) const PLAN_STYLES: [&str; 2] = ["cell", "frame"];

pub fn usage() -> String {
    format!(
        "Usage: {APP_NAME} autoreview <command> [options]

Review pull requests automatically, as the accounts gh is logged in to: a review starts when one of them is
requested as reviewer or @mentioned on a pull request, and is posted under that account.

Commands:
  run                          Poll and review until stopped (Ctrl+C, or stop the service)
  once                         One poll cycle: review what is due, then exit
  review <owner/repo#N | URL>  Review one pull request now
  review --repo-dir <dir> --base <sha> --head <sha>
                               Review a local diff with no GitHub access; with --json, one JSON object on stdout
  status                       Accounts, bans (when any), last poll, queue and recent reviews
  ban <login> [--account <login | host/login | *>]
                               Never review <login>'s pull requests, nor act on their review requests and mentions;
                               for every logged-in account (*) unless --account limits it to one
  unban <login> [--account <login | host/login | *>]
                               Lift that ban (default: the one for every account, *)
  bans                         The bans (autoreview.ban), by account
  doctor [--repo owner/name]   The sandbox test execution would use, and a self-check of its isolation; with --repo,
                               the local checkout whose environments would be bound into it
  prepare <repo-dir> [--python 3.x]
                               Build the repository's test environment now, with the network (uv virtualenv, npm ci,
                               mise toolchains), into the cache the sandbox binds read-only during reviews
  eval <repo-dir>              Measure the reviewer on the repository's history: review the commits later reverted or
                               fixed, and as many others as controls, and report how many it would have blocked
  register [--repo owner/name] The failure-mode register: what went wrong in the recorded reviews (rejected
                               findings, drops, what was not checked, the hits of the checks that caught their
                               planted bug), and Loki rules proposed from review rules findings keep citing (written
                               to <agent dir>/autoreview/loki-proposals.json for approval, never applied)
  install | uninstall          Write or remove the user service that runs \"{APP_NAME} autoreview run\"

Options:
  --dry-run                    Write the would-be review to <agent dir>/autoreview/dry-run/ instead of posting
  --json                       Machine-readable output (review, once, status, bans, eval, register)
  --account <login>            review: the account to review as (default: the first logged-in account of the host);
                               ban, unban: the reviewing account the ban is for (default: every account, *)
  --model <provider/model>     review: the model of the finder frames (default: autoreview.model, then review.model,
                               then rlm.frameModel, then the default model)
  --verify-model <p/m>         review: the model of the verifier frames (default: the finder model)
  --budget <tokens>            review: a token cap, e.g. 300000 or 300k (default: autoreview.budget, none)
  --thinking <level>           review: thinking level of the finder frames (off, minimal, low, medium, high, ...;
                               default: autoreview.thinking, low)
  --verify-thinking <level>    review: thinking level of the verifier frames (default: autoreview.verifyThinking, low)
  --mode <fast|deep|both|compiled|hybrid>
                               review: fast reviews the diff; deep investigates beyond it by read-only lookups in the
                               repository (nothing is executed); both (default) does one after the other, each
                               finding verified by a verifier frame. Experimental: hybrid runs both as discovery and
                               verifies each candidate with a host-written check program; compiled has one planner
                               write the whole program
  --verify-candidates <n>      review --mode hybrid (experimental): candidates verified at most (default:
                               autoreview.verifyCandidates, 12)
  --deep-model <p/m>           review: the model of the deep pass's investigators (default: the finder model)
  --finder-models <p/m,p/m>    review: the finder frames take these models in turn (default: all on --model)
  --deep-thinking <level>      review: their thinking level (default: autoreview.deepThinking, high; medium was
                               measured to find a third as much). Every --*-thinking default is per model: the
                               recommended model thinks high at every stage
  --verify-batch <n>           review: findings of one file judged by one verifier frame (default:
                               autoreview.verifyBatch, 4; 1 is one frame per finding)
  --plan-model <p/m>           review --mode compiled: the planner's model (default: autoreview.planModel, the finder model)
  --plan-thinking <level>      review --mode compiled: its thinking level (default: autoreview.planThinking; medium for
                               cells, high for a frame)
  --ask-model <p/m>            review --mode compiled: the small model the program's questions go to (default:
                               autoreview.askModel, the finder model)
  --ask-thinking <level>       review --mode compiled: its thinking level (default: autoreview.askThinking, low)
  --plan-style <cell|frame>    review --mode compiled: the planner as sandboxed Python cells over the rv API (default)
                               or as one JSON-program frame
  --plan-cells <n>             review --mode compiled: cells the planner may run (default: autoreview.planCells, 4)
  --dump-program <path>        review --mode compiled: save the validated program as JSON for inspection
  --program <path>             review --mode compiled: execute this saved program instead of calling the planner
  --run-tests | --no-run-tests review --repo-dir: let the deep pass run the project's tests in a sandbox (default:
                               autoreview.runTests, on); never without a sandbox
  --prepare                    review --repo-dir: prepare the environment first (as \"prepare\" does), then review
  --python <3.x>               prepare, review --prepare: the Python version to build the virtualenv with
  --test-env <dir>             review --repo-dir: a pre-built environment (virtualenv, node_modules) to bind read-only
  --checkout-roots <dir,...>   review: directories of local checkouts whose environments may serve the tests
                               (with --repo owner/name, whose remote the checkout must have)
  --guides <path,...>          review: private review guides (markdown files or directories)
  --block-at <level>           review: request changes from this level up (critical, high, medium, low, nit)
  --max-comments <n>           review: inline comments at most
  --deadline <seconds>         review: give up unfinished passes after this long and report the rest (default:
                               autoreview.deadlineSeconds, none)
  --list                       eval: list the cases and review nothing (no model calls)
  --limit <n>                  eval: reverted and fix-inducing commits reviewed at most (default 20)
  --controls <n>               eval: other commits reviewed as controls (default: as many as --limit)
  --scan <n>                   eval: commits of history searched for cases (default 2000)
  --only <sha,...>             eval: review only the cases whose commits start with these
  --labels <file>              eval: score recorded reviews against adjudicated labels instead (no review, no model
                               call); with <repo-dir>, check that each defect's fix comes after its head
  --packet                     eval --labels: print the blinded findings to label, and score nothing

Settings (global settings.json): autoreview.accounts, pollSeconds, concurrency, model, verifyModel, budget, dryRun,
frameConcurrency, modelConcurrency, mode, deepModel, deepThinking, deepRounds, referenceView, verifyBatch, planModel, planThinking, askModel, askThinking, planStyle, planCells, verifyCandidates, prepareEnvs, mise, blockAt, maxComments, runTests, testOwners, testRuns, testTimeoutSeconds, testEnv, testImage, checkoutRoots, guides, thinking, verifyThinking, deadlineSeconds, frameTimeoutSeconds, ack, ackLines, ackArt, signature, learn, skipBots, ban.
See docs/autoreview.md."
    )
}

/// A command-line mistake: exit code 2.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageError(pub String);

/// The parsed arguments (`Parsed` of cli.ts).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Parsed {
    pub command: String,
    pub target: Option<String>,
    pub dry_run: bool,
    pub json: bool,
    pub account: Option<String>,
    pub model: Option<String>,
    pub verify_model: Option<String>,
    pub budget: Option<f64>,
    pub thinking: Option<String>,
    pub verify_thinking: Option<String>,
    pub mode: Option<String>,
    pub deep_model: Option<String>,
    pub finder_models: Option<Vec<String>>,
    pub deep_thinking: Option<String>,
    pub plan_model: Option<String>,
    pub plan_thinking: Option<String>,
    pub ask_model: Option<String>,
    pub ask_thinking: Option<String>,
    pub plan_style: Option<String>,
    pub plan_cells: Option<f64>,
    pub verify_candidates: Option<f64>,
    pub verify_batch: Option<f64>,
    pub program_path: Option<String>,
    pub dump_program_path: Option<String>,
    pub deadline_seconds: Option<f64>,
    pub run_tests: Option<bool>,
    pub test_env: Option<String>,
    pub prepare: bool,
    pub python: Option<String>,
    pub repo: Option<String>,
    pub guides: Option<Vec<String>>,
    pub checkout_roots: Option<Vec<String>>,
    pub block_at: Option<String>,
    pub max_comments: Option<f64>,
    pub repo_dir: Option<String>,
    pub base: Option<String>,
    pub head: Option<String>,
    /// eval: list the cases only.
    pub list: bool,
    pub limit: Option<f64>,
    pub controls: Option<f64>,
    pub scan: Option<f64>,
    /// eval: the cases to review, by commit prefix.
    pub only: Option<Vec<String>>,
    /// eval: score recorded reviews against this labels file instead of reviewing (review::eval_labels).
    pub labels: Option<String>,
    /// eval --labels: print what the adjudicator sees of each case, and score nothing.
    pub packet: bool,
    pub help: bool,
}

fn whole(value: f64, min: f64) -> Option<f64> {
    (value.is_finite() && value.fract() == 0.0 && value >= min).then_some(value)
}

/// `parseTokens`: `300000`, `300k`, `1.5m` (commas and underscores ignored), at least the minimum budget.
pub fn parse_tokens(value: &str) -> Result<f64, UsageError> {
    let cleaned: String = value
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| !matches!(c, ',' | '_'))
        .collect();
    let refused = || {
        UsageError(format!(
            "--budget takes a token count such as 300000 or 300k, not {value}"
        ))
    };
    let (digits, unit) = match cleaned.strip_suffix('k') {
        Some(rest) => (rest, 1_000.0),
        None => match cleaned.strip_suffix('m') {
            Some(rest) => (rest, 1_000_000.0),
            None => (cleaned.as_str(), 1.0),
        },
    };
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !numeric(int) || (digits.contains('.') && !numeric(frac)) {
        return Err(refused());
    }
    let amount = (digits.parse::<f64>().map_err(|_| refused())? * unit).floor();
    if amount < MIN_BUDGET_TOKENS {
        return Err(UsageError(format!(
            "--budget must be at least {MIN_BUDGET_TOKENS} tokens"
        )));
    }
    Ok(amount)
}

fn is_python_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() <= 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
}

/// `parseAutoreviewArgs`.
pub fn parse_args(args: &[String]) -> Result<Parsed, UsageError> {
    let mut parsed = Parsed {
        command: args.first().cloned().unwrap_or_default(),
        ..Parsed::default()
    };
    if matches!(parsed.command.as_str(), "" | "--help" | "-h" | "help") {
        parsed.help = true;
        return Ok(parsed);
    }
    let mut index = 1;
    while index < args.len() {
        let arg = args[index].clone();
        let mut value = || -> Result<String, UsageError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))
        };
        let model = |ref_: String| -> Result<String, UsageError> {
            if model_scope::is_model_ref(&ref_) {
                Ok(ref_)
            } else {
                Err(UsageError(format!("{arg} takes provider/model, not {ref_}")))
            }
        };
        let list = |text: String| -> Vec<String> {
            text.split(',')
                .map(|item| item.trim().to_string())
                .filter(|item| !item.is_empty())
                .collect()
        };
        let thinking = |name: String| -> Result<String, UsageError> {
            if model_scope::THINKING_LEVELS.contains(&name.as_str()) {
                Ok(name)
            } else {
                Err(UsageError(format!(
                    "{arg} takes one of {}, not {name}",
                    model_scope::THINKING_LEVELS.join(", ")
                )))
            }
        };
        match arg.as_str() {
            "--help" | "-h" => parsed.help = true,
            "--dry-run" => parsed.dry_run = true,
            "--json" => parsed.json = true,
            "--account" => parsed.account = Some(value()?),
            "--model" => parsed.model = Some(model(value()?)?),
            "--verify-model" => parsed.verify_model = Some(model(value()?)?),
            "--budget" => parsed.budget = Some(parse_tokens(&value()?)?),
            "--thinking" => parsed.thinking = Some(thinking(value()?)?),
            "--verify-thinking" => parsed.verify_thinking = Some(thinking(value()?)?),
            "--deep-thinking" => parsed.deep_thinking = Some(thinking(value()?)?),
            "--deep-model" => parsed.deep_model = Some(model(value()?)?),
            "--finder-models" => {
                parsed.finder_models = Some(
                    value()?
                        .split(',')
                        .map(|name| model(name.trim().to_string()))
                        .collect::<Result<_, _>>()?,
                );
            }
            "--plan-model" => parsed.plan_model = Some(model(value()?)?),
            "--plan-thinking" => parsed.plan_thinking = Some(thinking(value()?)?),
            "--ask-model" => parsed.ask_model = Some(model(value()?)?),
            "--ask-thinking" => parsed.ask_thinking = Some(thinking(value()?)?),
            "--plan-style" => {
                let name = value()?;
                if !PLAN_STYLES.contains(&name.as_str()) {
                    return Err(UsageError(format!("--plan-style takes cell or frame, not {name}")));
                }
                parsed.plan_style = Some(name);
            }
            "--plan-cells" => {
                parsed.plan_cells = Some(
                    whole(jsrt::number::parse(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--plan-cells takes a whole number of cells".into()))?,
                );
            }
            "--verify-candidates" => {
                parsed.verify_candidates = Some(
                    whole(jsrt::number::parse(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--verify-candidates takes a whole number".into()))?,
                );
            }
            "--verify-batch" => {
                parsed.verify_batch = Some(
                    whole(jsrt::number::parse(&value()?), 1.0)
                        .ok_or_else(|| UsageError("--verify-batch takes a whole number".into()))?,
                );
            }
            "--program" => parsed.program_path = Some(value()?),
            "--dump-program" => parsed.dump_program_path = Some(value()?),
            "--run-tests" => parsed.run_tests = Some(true),
            "--no-run-tests" => parsed.run_tests = Some(false),
            "--test-env" => parsed.test_env = Some(value()?),
            "--prepare" => parsed.prepare = true,
            "--python" => {
                let version = value()?;
                if !is_python_version(&version) {
                    return Err(UsageError(format!(
                        "--python takes a version such as 3.12, not {version}"
                    )));
                }
                parsed.python = Some(version);
            }
            "--repo" => parsed.repo = Some(value()?),
            "--guides" => parsed.guides = Some(list(value()?)),
            "--checkout-roots" => parsed.checkout_roots = Some(list(value()?)),
            "--block-at" => {
                let name = value()?;
                if !LEVELS.contains(&name.as_str()) {
                    return Err(UsageError(format!(
                        "--block-at takes one of {}, not {name}",
                        LEVELS.join(", ")
                    )));
                }
                parsed.block_at = Some(name);
            }
            "--max-comments" => {
                parsed.max_comments = Some(
                    whole(jsrt::number::parse(&value()?), 0.0)
                        .ok_or_else(|| UsageError("--max-comments takes a whole number".into()))?,
                );
            }
            "--mode" => {
                let name = value()?;
                if !REVIEW_MODES.contains(&name.as_str()) {
                    return Err(UsageError(format!(
                        "--mode takes fast, deep, both, compiled, hybrid, auto or minimal, not {name}"
                    )));
                }
                parsed.mode = Some(name);
            }
            "--deadline" => {
                parsed.deadline_seconds = Some(
                    whole(jsrt::number::parse(&value()?), 0.0)
                        .ok_or_else(|| UsageError("--deadline takes whole seconds (0 for no deadline)".into()))?,
                );
            }
            "--repo-dir" => parsed.repo_dir = Some(value()?),
            "--list" => parsed.list = true,
            "--labels" => parsed.labels = Some(value()?),
            "--packet" => parsed.packet = true,
            "--only" => parsed.only = Some(list(value()?)),
            "--limit" | "--controls" | "--scan" => {
                let count = whole(
                    jsrt::number::parse(&value()?),
                    if arg == "--controls" { 0.0 } else { 1.0 },
                )
                .ok_or_else(|| UsageError(format!("{arg} takes a whole number")))?;
                match arg.as_str() {
                    "--limit" => parsed.limit = Some(count),
                    "--controls" => parsed.controls = Some(count),
                    _ => parsed.scan = Some(count),
                }
            }
            "--base" => parsed.base = Some(value()?),
            "--head" => parsed.head = Some(value()?),
            _ if arg.starts_with('-') => {
                return Err(UsageError(format!("unknown option for {APP_NAME} autoreview: {arg}")));
            }
            _ if parsed.target.is_none()
                && matches!(parsed.command.as_str(), "review" | "prepare" | "ban" | "unban" | "eval") =>
            {
                parsed.target = Some(arg.clone());
            }
            _ => return Err(UsageError(format!("unexpected argument: {arg}"))),
        }
        index += 1;
    }
    Ok(parsed)
}

/// `logoText`: Ultron's half-size logo, trimmed of blank rows and common indentation (the acknowledgement's art).
pub fn logo_text() -> String {
    let mut rows: Vec<String> = interactive::header::halve_logo(interactive::header::ULTRON_LOGO)
        .into_iter()
        .map(|row| row.replace('$', " ").trim_end().to_string())
        .collect();
    while rows.first().is_some_and(String::is_empty) {
        rows.remove(0);
    }
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    let indent = rows
        .iter()
        .filter(|row| !row.is_empty())
        .map(|row| row.len() - row.trim_start().len())
        .min()
        .unwrap_or(0);
    rows.iter()
        .map(|row| row.get(indent..).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The secrets this process knows (`output-secrets.ts` `knownSecrets`, module-global): every gh token the token
/// store reads is registered here (`registerSecretValues`), and every autoreview log line is masked with it.
pub(super) fn known_secrets() -> Arc<host::secrets::OutputMasker> {
    static MASKER: std::sync::OnceLock<Arc<host::secrets::OutputMasker>> = std::sync::OnceLock::new();
    MASKER.get_or_init(Arc::default).clone()
}

/// Log lines (`createLogger`): `<ISO time> <line with secrets masked>` on the process's stderr as they happen (and,
/// for the service, in the day's log file).
pub(super) fn logger(var: &crate::VarLookup) -> Log {
    let mask = var(session_core::compose::MASK_SECRETS_ENV);
    let masker = known_secrets();
    Arc::new(move |line: &str| {
        let (masked, _) = masker.mask_counted(line, mask.as_deref());
        eprintln!("{} {masked}", jsrt::date::iso_string(jsrt::date::now_ms()));
    })
}

/// The settings `ultron autoreview` reads (global only: a reviewed repository cannot change them).
pub(super) struct Saved {
    autoreview: AutoreviewSettings,
    retry: ai::retry::RetryPolicy,
    enabled: Option<bool>,
    fallbacks_review_model: Option<String>,
    frame_model: Option<String>,
    frame_thinking: Option<String>,
    child_model: Option<String>,
    map_tokens: Option<u64>,
    default_provider: Option<String>,
    default_model: Option<String>,
}

pub(super) fn saved_settings(env: &Env) -> Saved {
    let agent_dir = env.agent_dir();
    let manager = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
    let raw = Value::Object(review::settings::saved(manager.global_settings()));
    let autoreview: AutoreviewSettings = serde_json::from_value(raw.clone()).unwrap_or_default();
    let rlm = config::keys::rlm_model_settings(manager.global_settings());
    let text = |value: Option<&Value>| value.and_then(Value::as_str).map(str::to_string);
    Saved {
        retry: ai::retry::RetryPolicy::from_settings(&manager.retry_settings(), true),
        enabled: raw.get("enabled").and_then(Value::as_bool),
        fallbacks_review_model: config::keys::review_model(manager.global_settings()),
        frame_model: rlm.frame_model,
        frame_thinking: rlm.frame_thinking,
        child_model: rlm.child_model.filter(|model| !model.starts_with("claude-code/")),
        map_tokens: rlm.map_tokens,
        default_provider: text(manager.default_provider().as_ref()),
        default_model: text(manager.default_model().as_ref()),
        autoreview,
    }
}

/// `hasModel`: a model of the user's own catalog (models.json), or Claude Code's when its CLI would be found.
pub(super) fn has_model(env: &Env, reference: &str) -> bool {
    if reference.starts_with("claude-code/") {
        let mut map = BTreeMap::new();
        for key in [ai::claude_code_cli::CLAUDE_CODE_BIN_ENV, "PATH"] {
            if let Some(value) = (env.var)(key) {
                map.insert(key.to_string(), value);
            }
        }
        return ai::claude_code_cli::resolve_claude_cli(&map).is_ok();
    }
    let Some((provider, id)) = reference.split_once('/') else {
        return false;
    };
    if provider.is_empty() {
        return false;
    }
    let path = config::paths::AgentPaths::new(env.agent_dir()).models();
    let catalog = ai::model_config::ModelConfig::load_with_home(path.to_str(), &env.home);
    catalog
        .provider(provider)
        .and_then(|value| value.get("models"))
        .and_then(Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|model| model.get("id").and_then(Value::as_str) == Some(id))
        })
}

/// The configuration of this run: the saved settings with the command line's overrides.
pub(super) fn configure(parsed: &Parsed, saved: &Saved, env: &Env) -> AutoreviewConfig {
    let mut settings = saved.autoreview.clone();
    let set = |target: &mut Option<String>, value: &Option<String>| {
        if value.is_some() {
            *target = value.clone();
        }
    };
    set(&mut settings.model, &parsed.model);
    set(&mut settings.verify_model, &parsed.verify_model);
    if parsed.budget.is_some() {
        settings.budget = parsed.budget;
    }
    set(&mut settings.thinking, &parsed.thinking);
    set(&mut settings.verify_thinking, &parsed.verify_thinking);
    set(&mut settings.mode, &parsed.mode);
    if parsed.guides.is_some() {
        settings.guides = parsed.guides.clone();
    }
    if parsed.checkout_roots.is_some() {
        settings.checkout_roots = parsed.checkout_roots.clone();
    }
    set(&mut settings.block_at, &parsed.block_at);
    if parsed.max_comments.is_some() {
        settings.max_comments = parsed.max_comments;
    }
    set(&mut settings.deep_model, &parsed.deep_model);
    if parsed.finder_models.is_some() {
        settings.finder_models = parsed.finder_models.clone();
    }
    set(&mut settings.deep_thinking, &parsed.deep_thinking);
    set(&mut settings.plan_model, &parsed.plan_model);
    set(&mut settings.plan_thinking, &parsed.plan_thinking);
    set(&mut settings.ask_model, &parsed.ask_model);
    set(&mut settings.ask_thinking, &parsed.ask_thinking);
    set(&mut settings.plan_style, &parsed.plan_style);
    if parsed.plan_cells.is_some() {
        settings.plan_cells = parsed.plan_cells;
    }
    if parsed.verify_candidates.is_some() {
        settings.verify_candidates = parsed.verify_candidates;
    }
    if parsed.verify_batch.is_some() {
        settings.verify_batch = parsed.verify_batch;
    }
    if parsed.deadline_seconds.is_some() {
        settings.deadline_seconds = parsed.deadline_seconds;
    }
    let has = |reference: &str| has_model(env, reference);
    let fallbacks = ModelFallbacks {
        review_model: saved.fallbacks_review_model.clone(),
        frame_model: saved.frame_model.clone(),
        default_provider: saved.default_provider.clone(),
        default_model: saved.default_model.clone(),
        has_model: Some(&has),
    };
    let mut config = resolve_config(&settings, &fallbacks, &logo_text());
    config.dry_run = parsed.dry_run || config.dry_run;
    config
}

/// The kernel-backed engine on the configured model; models the frames name resolve as the session's do: the
/// built-in catalog and models.json (custom providers and models, their keys and headers) with the profile's
/// credentials.
pub(super) fn open_engine(
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    dir: &Path,
    log: Log,
) -> Result<KernelEngine, String> {
    let resolver_env = env.clone();
    let resolve: ModelResolver = Arc::new(move |reference: &str| {
        let flags = ModelFlags {
            model: Some(reference.to_string()),
            ..ModelFlags::default()
        };
        let model = session_core::lane::resolve_model(&flags, &resolver_env)?;
        let key = session_core::lane::api_key(&flags, &resolver_env, &model.provider);
        Ok((model, key))
    });
    let reference = config
        .model
        .clone()
        .ok_or("no model for autoreview: set autoreview.model or a default model (ultron --model, /model)")?;
    let (model, api_key) = resolve(&reference)?;
    let options = EngineOptions {
        dir: dir.to_path_buf(),
        runtime_path: env.runtime(),
        python: None,
        model,
        api_key,
        resolve,
        transport: Arc::new(ai::ReqwestTransport::for_env(&*env.var, None)),
        retry: saved.retry,
        frame_env: inference::frame_env(&*env.var),
        settings: inference::ModelSettings {
            frame_model: saved.frame_model.clone(),
            frame_thinking: saved.frame_thinking.clone(),
            child_model: saved.child_model.clone(),
            review_model: saved.fallbacks_review_model.clone(),
            map_tokens: saved.map_tokens,
        },
        kernel_env: Vec::new(),
        modules: {
            let mut mods: Vec<Arc<dyn kernel_apis::ApiModule>> = vec![Arc::new(judge_module(env))];
            if let Some(memory) = memory_module(env, dir) {
                mods.push(memory);
            }
            mods
        },
        base_env: Some(session_core::compose::session_environment(&*env.var)),
        log: Some(log),
    };
    KernelEngine::open(options)
}

/// `judge.decide` for the review engine: the profile's seat as `ultron judge` builds it (global `judge.*` settings,
/// the services' keys, the fallback and escalation models), rebuilt when the settings change.
pub(super) fn judge_module(env: &Env) -> judge::JudgeModule {
    let (settings_env, seat_env) = (env.clone(), env.clone());
    judge::JudgeModule::new(
        Arc::new(move || {
            let settings =
                config::settings::SettingsManager::create(&settings_env.cwd, &settings_env.agent_dir(), false);
            config::keys::judge_settings(settings.global_settings())
        }),
        Arc::new(move |_settings| crate::judge_cli::seat(&seat_env, true, None)),
    )
}

/// Optional `memory.*` host module backed by memorantado when configured and reachable; `None` when disabled or offline.
fn memory_module(env: &Env, dir: &Path) -> Option<Arc<dyn kernel_apis::ApiModule>> {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let endpoint = memorantado::Endpoint::configured(&*env.var, &settings)?;
    if !memorantado::resolve::probe(&endpoint.url) {
        return None;
    }
    let cwd = dir.display().to_string();
    let home = env.home.display().to_string();
    let backend = endpoint.session_backend("autoreview", &cwd, &home).ok()?;
    let doc = Arc::new(kernel::store::MemoryDocumentStore::default());
    let service = kernel_apis::MemoryService::new(doc, Arc::new(backend), kernel_apis::worker_gate())
        .ok()?
        .with_root(dir.to_path_buf());
    Some(Arc::new(kernel_apis::MemoryApi { service: Some(service) }))
}

pub(super) fn absolute(cwd: &Path, path: &str) -> PathBuf {
    config::paths::normalize(&cwd.join(path))
}

/// `expandPath`: `~` and relative paths made absolute.
pub fn expand_path(path: &str, home: &str, cwd: &Path) -> String {
    let text = path.trim();
    if text == "~" {
        return home.to_string();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return jsrt::path::join(&[home, rest]);
    }
    absolute(cwd, text).display().to_string()
}

pub(super) enum Failure {
    Usage(String),
    Error(String),
}

impl From<UsageError> for Failure {
    fn from(error: UsageError) -> Self {
        Failure::Usage(error.0)
    }
}

/// Runs `ultron autoreview ...`; None when `argv` is another command.
pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<i32> {
    if argv.first().map(String::as_str) != Some("autoreview") {
        return None;
    }
    Some(run_command(&argv[1..], env, out, err))
}

/// `runAutoreviewCommand`: the exit code (2 for a usage error).
pub fn run_command(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_command_with(args, env, &Overrides::default(), out, err)
}

/// [`run_command`] with `gh`/git and the engine replaced.
pub fn run_command_with(args: &[String], env: &Env, ov: &Overrides, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(UsageError(message)) => {
            let _ = writeln!(err, "Error: {message}");
            return 2;
        }
    };
    if parsed.help {
        let _ = writeln!(out, "{}", usage());
        return 0;
    }
    let outcome = execute(&parsed, env, ov, out, err);
    match outcome {
        Ok(code) => code,
        Err(Failure::Usage(message)) => {
            let _ = writeln!(err, "Error: {message}");
            2
        }
        Err(Failure::Error(message)) => {
            let _ = writeln!(err, "Error: {message}");
            1
        }
    }
}

/// The reviewer's dependencies over the real `gh`, git, state, engine and (with `learn`) Hindsight.
pub(super) fn reviewer_deps(
    config: &AutoreviewConfig,
    saved: &Saved,
    env: &Env,
    paths: &review::settings::AutoreviewPaths,
    ov: &Overrides,
) -> Result<review::reviewer::ReviewerDeps, Failure> {
    let log = logger(&env.var);
    let engine: Arc<dyn ReviewEngine> = match &ov.engine {
        Some(engine) => engine.clone(),
        None => Arc::new(open_engine(config, saved, env, Path::new(&paths.dir), log.clone()).map_err(Failure::Error)?),
    };
    let runner = ov.runner();
    let runner_for_envs = runner.clone();
    let lessons: Option<Arc<dyn review::reviewer::LessonMemory>> = if config.learn {
        let agent_dir = env.agent_dir();
        let manager = config::settings::SettingsManager::create(&env.cwd, &agent_dir, false);
        let (url, _) = hindsight::resolve::resolve_in(&*env.var, &manager);
        url.and_then(|url| review::lessons::HindsightLessons::new(&url, Box::new(jsrt::date::now_ms), None).ok())
            .map(|lessons| Arc::new(lessons) as Arc<dyn review::reviewer::LessonMemory>)
    } else {
        None
    };
    Ok(review::reviewer::ReviewerDeps {
        runner: runner.clone(),
        tokens: Arc::new(
            review::accounts::TokenStore::new(runner.clone()).with_secret_sink(Arc::new(|token: &str| {
                known_secrets().register_secret_values(&[Value::from(token)]);
            })),
        ),
        engine,
        store: Arc::new(review::state::StateStore::new(&paths.state)),
        checkouts: Arc::new(review::checkout::CheckoutManager::new(runner, &paths.cache)),
        config: config.clone(),
        paths: paths.clone(),
        log,
        now: Arc::new(jsrt::date::now_ms),
        random: Arc::new(random),
        lessons,
        environments: Some(Arc::new(review::prepare_env::RunnerPreparer {
            runner: runner_for_envs,
            cache_dir: paths.cache.clone(),
            mise: Some(config.mise.clone()),
            env: prepare_vars(env),
            home: env.home.display().to_string(),
            now: Arc::new(jsrt::date::now_ms),
        })),
        home: env.home.display().to_string(),
        bans: Arc::new(review::ban::Bans::reloading(ban_loader(env))),
    })
}

/// `autoreview.ban` as the global settings file says now (a repository's settings cannot set it).
pub(super) fn ban_loader(env: &Env) -> review::ban::BanLoader {
    let (cwd, agent_dir) = (env.cwd.clone(), env.agent_dir());
    Arc::new(move || {
        let mut manager = config::settings::SettingsManager::create(&cwd, &agent_dir, false);
        if !manager.drain_errors().is_empty() {
            return None;
        }
        Some(review::ban::BanList::from_value(
            review::settings::saved(manager.global_settings()).get("ban"),
        ))
    })
}

/// A number in [0, 1) (`Math.random`), for the acknowledgement line.
fn random() -> f64 {
    let mut bytes = [0u8; 8];
    if getrandom_fill(&mut bytes).is_err() {
        return 0.0;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

fn getrandom_fill(bytes: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(bytes)
}

fn execute(
    parsed: &Parsed,
    env: &Env,
    ov: &Overrides,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    if matches!(parsed.command.as_str(), "ban" | "unban" | "bans") {
        return ban_command(parsed, env, out);
    }
    let saved = saved_settings(env);
    let config = configure(parsed, &saved, env);
    let process_env: HashMap<String, String> = [review::settings::CACHE_DIR_ENV, "XDG_CACHE_HOME"]
        .into_iter()
        .filter_map(|key| (env.var)(key).map(|value| (key.to_string(), value)))
        .collect();
    let paths = autoreview_paths(
        &env.agent_dir().display().to_string(),
        &process_env,
        &env.home.display().to_string(),
    );
    match parsed.command.as_str() {
        "review" => {}
        "run" | "once" => {
            if saved.enabled == Some(false) {
                let _ = writeln!(
                    err,
                    "autoreview is off (autoreview.enabled: false; /autoreview on or {APP_NAME} autoreview is not needed for one review)"
                );
                return Ok(0);
            }
            return daemon_command(parsed, &config, &saved, env, &paths, ov, out);
        }
        "install" | "uninstall" => return service(parsed, &config, env, out),
        "status" => {
            let bans = crate::autoreview_ban::current(env);
            return status(parsed, &config, &paths.state, &bans, ov, out);
        }
        "doctor" => return doctor(parsed, &config, &model_lines(&config), env, &paths.cache, ov, out),
        "prepare" => return prepare_command(parsed, &config, env, &paths.cache, ov, out, err),
        "eval" if parsed.labels.is_some() => return labels_command(parsed, env, ov, out, err),
        "eval" => return eval_command(parsed, &config, &saved, env, &paths, ov, out, err),
        "register" => return register_command(parsed, &paths, out),
        other => {
            return Err(Failure::Usage(format!("unknown command: {other}\n\n{}", usage())));
        }
    }
    let Some(repo_dir) = &parsed.repo_dir else {
        return review_pull_request(parsed, &config, &saved, env, &paths, ov, out);
    };
    let log = logger(&env.var);
    let dir = PathBuf::from(&paths.dir);
    if parsed.target.is_some() {
        return Err(UsageError("give a pull request or --repo-dir, not both".into()).into());
    }
    let (Some(base), Some(head)) = (&parsed.base, &parsed.head) else {
        return Err(UsageError("--repo-dir needs --base and --head".into()).into());
    };
    if base.is_empty() || head.is_empty() {
        return Err(UsageError("--repo-dir needs --base and --head".into()).into());
    }
    if (parsed.program_path.is_some() || parsed.dump_program_path.is_some()) && config.mode != "compiled" {
        return Err(UsageError("--program and --dump-program need --mode compiled".into()).into());
    }
    let repo_dir = absolute(&env.cwd, repo_dir);
    let lent = match &parsed.repo {
        Some(repo) if !config.checkout_roots.is_empty() => {
            review::github::parse_pull_target(&format!("{repo}#1"), "github.com")
                .and_then(|named| review::local::find_checkout(&*ov.runner(), &config.checkout_roots, &named))
        }
        _ => None,
    };
    let prepared = if parsed.prepare {
        let prepared = run_prepare(
            &repo_dir.display().to_string(),
            parsed,
            &config,
            env,
            &paths.cache,
            ov,
            err,
            false,
        )?;
        let mut lines = vec![format!(
            "prepared environment {}{}: {}",
            prepared.hash,
            if prepared.cached { " (cached)" } else { "" },
            if prepared.prepared.is_empty() {
                "nothing".to_string()
            } else {
                prepared.prepared.join("; ")
            }
        )];
        lines.extend(prepared.failures.iter().map(|item| format!("prepare: {item}")));
        for line in lines {
            let _ = writeln!(err, "{line}");
        }
        Some(prepared)
    } else {
        None
    };
    let use_prepared = prepared.as_ref().filter(|prepared| !prepared.prepared.is_empty());
    let (engine, start_ms): (Arc<dyn ReviewEngine>, Option<u64>) = match &ov.engine {
        Some(engine) => (engine.clone(), None),
        None => {
            let engine = open_engine(&config, &saved, env, &dir, log).map_err(Failure::Error)?;
            let start_ms = engine.start_ms;
            (Arc::new(engine), Some(start_ms))
        }
    };
    let mut spec = offline_spec(parsed, &config, env, &repo_dir, lent.as_deref(), use_prepared);
    let head = parsed
        .head
        .clone()
        .filter(|head| !head.is_empty())
        .unwrap_or_else(|| "HEAD".into());
    index_knowledge(parsed, &paths.knowledge, &repo_dir, &head, &mut spec, err);
    let result = engine.review(&spec, None).map_err(Failure::Error)?;
    let io = |error: std::io::Error| Failure::Error(error.to_string());
    if parsed.json {
        let max_comments = Some(config.max_comments.0 as i64);
        let json = offline_json(&result, start_ms, Some(&config.block_at), max_comments).map_err(Failure::Error)?;
        writeln!(out, "{}", jsrt::json::stringify(&json)).map_err(io)?;
    } else {
        let typed = EngineResult::parse(&result).map_err(Failure::Error)?;
        let plan = plan_review(
            &typed,
            &PlanOptions {
                self_authored: false,
                state: "open".into(),
                head_sha: head.clone(),
                since_sha: None,
                signature: config.signature,
                block_at: Some(config.block_at.clone()),
                never_request_changes: config.never_request_changes,
                max_comments: Some(config.max_comments.0 as i64),
                promotion: None,
                fresh_lines: None,
            },
        );
        writeln!(out, "{}", plan.body).map_err(io)?;
    }
    Ok(0)
}
