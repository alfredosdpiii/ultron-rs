//! Argument parsing for `ultron autoreview`.

use review::settings::MIN_BUDGET_TOKENS;

use super::{APP_NAME, LEVELS, PLAN_STYLES, REVIEW_MODES};

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

fn next_value(args: &[String], index: &mut usize, arg: &str) -> Result<String, UsageError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| UsageError(format!("{arg} needs a value")))
}

fn parse_model_ref(arg: &str, reference: String) -> Result<String, UsageError> {
    if model_scope::is_model_ref(&reference) {
        Ok(reference)
    } else {
        Err(UsageError(format!("{arg} takes provider/model, not {reference}")))
    }
}

fn parse_thinking_level(arg: &str, name: String) -> Result<String, UsageError> {
    if model_scope::THINKING_LEVELS.contains(&name.as_str()) {
        Ok(name)
    } else {
        Err(UsageError(format!(
            "{arg} takes one of {}, not {name}",
            model_scope::THINKING_LEVELS.join(", ")
        )))
    }
}

fn parse_csv(text: String) -> Vec<String> {
    text.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn parse_whole_arg(args: &[String], index: &mut usize, arg: &str, min: f64, message: &str) -> Result<f64, UsageError> {
    let raw = next_value(args, index, arg)?;
    whole(jsrt::number::parse(&raw), min).ok_or_else(|| UsageError(message.into()))
}

fn parse_choice_arg(
    args: &[String],
    index: &mut usize,
    arg: &str,
    allowed: &[&str],
    expected: &str,
) -> Result<String, UsageError> {
    let name = next_value(args, index, arg)?;
    if !allowed.contains(&name.as_str()) {
        return Err(UsageError(format!("{arg} takes {expected}, not {name}")));
    }
    Ok(name)
}

fn apply_flag_option(parsed: &mut Parsed, arg: &str, _args: &[String], _index: &mut usize) -> Result<bool, UsageError> {
    match arg {
        "--help" | "-h" => parsed.help = true,
        "--dry-run" => parsed.dry_run = true,
        "--json" => parsed.json = true,
        "--run-tests" => parsed.run_tests = Some(true),
        "--no-run-tests" => parsed.run_tests = Some(false),
        "--prepare" => parsed.prepare = true,
        "--list" => parsed.list = true,
        "--packet" => parsed.packet = true,
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_core_string_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    let slot = match arg {
        "--account" => &mut parsed.account,
        "--repo" => &mut parsed.repo,
        "--repo-dir" => &mut parsed.repo_dir,
        "--base" => &mut parsed.base,
        "--head" => &mut parsed.head,
        _ => return Ok(false),
    };
    *slot = Some(next_value(args, index, arg)?);
    Ok(true)
}

fn apply_path_string_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    let slot = match arg {
        "--program" => &mut parsed.program_path,
        "--dump-program" => &mut parsed.dump_program_path,
        "--test-env" => &mut parsed.test_env,
        "--labels" => &mut parsed.labels,
        _ => return Ok(false),
    };
    *slot = Some(next_value(args, index, arg)?);
    Ok(true)
}

fn apply_finder_models_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    if arg != "--finder-models" {
        return Ok(false);
    }
    let raw = next_value(args, index, arg)?;
    parsed.finder_models = Some(
        raw.split(',')
            .map(|name| parse_model_ref(arg, name.trim().to_string()))
            .collect::<Result<_, _>>()?,
    );
    Ok(true)
}

fn apply_model_option(parsed: &mut Parsed, arg: &str, args: &[String], index: &mut usize) -> Result<bool, UsageError> {
    let slot = match arg {
        "--model" => &mut parsed.model,
        "--verify-model" => &mut parsed.verify_model,
        "--deep-model" => &mut parsed.deep_model,
        "--plan-model" => &mut parsed.plan_model,
        "--ask-model" => &mut parsed.ask_model,
        _ => return Ok(false),
    };
    *slot = Some(parse_model_ref(arg, next_value(args, index, arg)?)?);
    Ok(true)
}

fn apply_thinking_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    let slot = match arg {
        "--thinking" => &mut parsed.thinking,
        "--verify-thinking" => &mut parsed.verify_thinking,
        "--deep-thinking" => &mut parsed.deep_thinking,
        "--plan-thinking" => &mut parsed.plan_thinking,
        "--ask-thinking" => &mut parsed.ask_thinking,
        _ => return Ok(false),
    };
    *slot = Some(parse_thinking_level(arg, next_value(args, index, arg)?)?);
    Ok(true)
}

fn apply_list_option(parsed: &mut Parsed, arg: &str, args: &[String], index: &mut usize) -> Result<bool, UsageError> {
    let slot = match arg {
        "--guides" => &mut parsed.guides,
        "--checkout-roots" => &mut parsed.checkout_roots,
        "--only" => &mut parsed.only,
        _ => return Ok(false),
    };
    *slot = Some(parse_csv(next_value(args, index, arg)?));
    Ok(true)
}

fn apply_validated_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    match arg {
        "--plan-style" => {
            parsed.plan_style = Some(parse_choice_arg(args, index, arg, &PLAN_STYLES, "cell or frame")?);
        }
        "--block-at" => {
            let expected = format!("one of {}", LEVELS.join(", "));
            parsed.block_at = Some(parse_choice_arg(args, index, arg, &LEVELS, &expected)?);
        }
        "--mode" => {
            parsed.mode = Some(parse_choice_arg(
                args,
                index,
                arg,
                &REVIEW_MODES,
                "fast, deep, both, compiled, hybrid, auto or minimal",
            )?);
        }
        "--python" => {
            let version = next_value(args, index, arg)?;
            if !is_python_version(&version) {
                return Err(UsageError(format!(
                    "--python takes a version such as 3.12, not {version}"
                )));
            }
            parsed.python = Some(version);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn apply_budget_option(parsed: &mut Parsed, arg: &str, args: &[String], index: &mut usize) -> Result<bool, UsageError> {
    if arg != "--budget" {
        return Ok(false);
    }
    parsed.budget = Some(parse_tokens(&next_value(args, index, arg)?)?);
    Ok(true)
}

fn apply_numeric_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    let (min, message, slot) = match arg {
        "--plan-cells" => (
            1.0,
            "--plan-cells takes a whole number of cells",
            &mut parsed.plan_cells,
        ),
        "--verify-candidates" => (
            1.0,
            "--verify-candidates takes a whole number",
            &mut parsed.verify_candidates,
        ),
        "--verify-batch" => (1.0, "--verify-batch takes a whole number", &mut parsed.verify_batch),
        "--max-comments" => (0.0, "--max-comments takes a whole number", &mut parsed.max_comments),
        "--deadline" => (
            0.0,
            "--deadline takes whole seconds (0 for no deadline)",
            &mut parsed.deadline_seconds,
        ),
        _ => return Ok(false),
    };
    *slot = Some(parse_whole_arg(args, index, arg, min, message)?);
    Ok(true)
}

fn apply_eval_count_option(
    parsed: &mut Parsed,
    arg: &str,
    args: &[String],
    index: &mut usize,
) -> Result<bool, UsageError> {
    let (min, slot) = match arg {
        "--limit" => (1.0, &mut parsed.limit),
        "--controls" => (0.0, &mut parsed.controls),
        "--scan" => (1.0, &mut parsed.scan),
        _ => return Ok(false),
    };
    let message = format!("{arg} takes a whole number");
    *slot = Some(parse_whole_arg(args, index, arg, min, &message)?);
    Ok(true)
}

fn apply_positional(parsed: &mut Parsed, arg: &str) -> Result<(), UsageError> {
    if arg.starts_with('-') {
        return Err(UsageError(format!("unknown option for {APP_NAME} autoreview: {arg}")));
    }
    if parsed.target.is_none() && matches!(parsed.command.as_str(), "review" | "prepare" | "ban" | "unban" | "eval") {
        parsed.target = Some(arg.to_string());
        return Ok(());
    }
    Err(UsageError(format!("unexpected argument: {arg}")))
}

type OptionHandler = fn(&mut Parsed, &str, &[String], &mut usize) -> Result<bool, UsageError>;

const OPTION_HANDLERS: &[OptionHandler] = &[
    apply_flag_option,
    apply_core_string_option,
    apply_path_string_option,
    apply_finder_models_option,
    apply_model_option,
    apply_thinking_option,
    apply_list_option,
    apply_validated_option,
    apply_budget_option,
    apply_numeric_option,
    apply_eval_count_option,
];

fn parse_one_arg(parsed: &mut Parsed, args: &[String], index: &mut usize) -> Result<(), UsageError> {
    let arg = args[*index].clone();
    for handler in OPTION_HANDLERS {
        if handler(parsed, &arg, args, index)? {
            return Ok(());
        }
    }
    apply_positional(parsed, &arg)
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
        parse_one_arg(&mut parsed, args, &mut index)?;
        index += 1;
    }
    Ok(parsed)
}
