use crate::UsageError;
use crate::claude_cli::resolve_claude_binary;
use crate::guide::UNLIMITED;
use crate::settings::ChildrenMode;
use config::providers::CLAUDE_CODE_DEFAULT_MODEL;
use std::path::Path;

pub const CLAUDE_CHILDREN_ENV: &str = "ULTRON_CLAUDE_CHILDREN";

pub fn default_frame_model() -> String {
    crate::claude_code_ref(CLAUDE_CODE_DEFAULT_MODEL)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerArgs {
    pub child: bool,
    pub parent_socket: Option<String>,
    pub parent_name: Option<String>,
    pub parent_lane: Option<String>,
    pub level: u64,
    /// [`UNLIMITED`] for a root server.
    pub allowance: u64,
    pub frame_model: Option<String>,
    pub children: Option<ChildrenMode>,
    pub child_model: Option<String>,
    pub ultron_child_model: Option<String>,
    /// `--root-model`: the Claude Code model the root runs on, as the launcher passed it to `claude --model` (only
    /// when it is not the default, [`config::providers::CLAUDE_CODE_DEFAULT_MODEL`]).
    pub root_model: Option<String>,
    pub socket: Option<String>,
    pub instructions: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
    /// `--wake`: the launcher's wait-events hook wakes the idle root when its work ends (crate::hooks::HookEvent::WaitEvents).
    pub wake: bool,
}

fn parse_non_negative_int(text: &str, flag: &str) -> Result<u64, UsageError> {
    let number = jsrt::number::parse(text);
    if !(jsrt::number::is_safe_integer(number) && number >= 0.0) {
        return Err(UsageError(format!("{flag} must be a non-negative integer")));
    }
    Ok(number as u64)
}

fn next_flag_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, UsageError> {
    *index += 1;
    args.get(*index)
        .cloned()
        .ok_or_else(|| UsageError(format!("{flag} needs a value")))
}

fn apply_bool_flag(arg: &str, parsed: &mut McpServerArgs) -> bool {
    match arg {
        "--child" => parsed.child = true,
        "--no-instructions" => parsed.instructions = false,
        "--wake" => parsed.wake = true,
        "--no-context-files" => parsed.no_context_files = true,
        "--no-skills" => parsed.no_skills = true,
        _ => return false,
    }
    true
}

fn apply_string_flag(
    arg: &str,
    args: &[String],
    index: &mut usize,
    parsed: &mut McpServerArgs,
) -> Result<bool, UsageError> {
    let slot = match arg {
        "--parent-socket" => &mut parsed.parent_socket,
        "--parent-name" => &mut parsed.parent_name,
        "--parent-lane" => &mut parsed.parent_lane,
        "--frame-model" => &mut parsed.frame_model,
        "--child-model" => &mut parsed.child_model,
        "--ultron-child-model" => &mut parsed.ultron_child_model,
        "--root-model" => &mut parsed.root_model,
        "--socket" => &mut parsed.socket,
        _ => return Ok(false),
    };
    *slot = Some(next_flag_value(args, index, arg)?);
    Ok(true)
}

fn parse_count_flag(args: &[String], index: &mut usize, flag: &str) -> Result<u64, UsageError> {
    parse_non_negative_int(&next_flag_value(args, index, flag)?, flag)
}

fn parse_children_flag(args: &[String], index: &mut usize) -> Result<ChildrenMode, UsageError> {
    let raw = next_flag_value(args, index, "--children")?;
    ChildrenMode::parse(&raw).ok_or_else(|| UsageError("--children must be claude or ultron".into()))
}

fn apply_typed_flag(
    arg: &str,
    args: &[String],
    index: &mut usize,
    parsed: &mut McpServerArgs,
) -> Result<bool, UsageError> {
    match arg {
        "--level" => parsed.level = parse_count_flag(args, index, arg)?,
        "--allowance" => parsed.allowance = parse_count_flag(args, index, arg)?,
        "--children" => parsed.children = Some(parse_children_flag(args, index)?),
        _ => return Ok(false),
    }
    Ok(true)
}

/// Parse `ultron mcp` flags; unknown flags are an error.
pub fn parse_mcp_args(args: &[String]) -> Result<McpServerArgs, UsageError> {
    let mut parsed = McpServerArgs {
        child: false,
        parent_socket: None,
        parent_name: None,
        parent_lane: None,
        level: 0,
        allowance: UNLIMITED,
        frame_model: None,
        children: None,
        child_model: None,
        ultron_child_model: None,
        root_model: None,
        socket: None,
        instructions: true,
        no_context_files: false,
        no_skills: false,
        wake: false,
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if !apply_bool_flag(arg, &mut parsed)
            && !apply_string_flag(arg, args, &mut index, &mut parsed)?
            && !apply_typed_flag(arg, args, &mut index, &mut parsed)?
        {
            return Err(UsageError(format!("unknown option for ultron mcp: {arg}")));
        }
        index += 1;
    }
    if parsed.child && (parsed.parent_socket.is_none() || parsed.parent_lane.is_none()) {
        return Err(UsageError(
            "ultron mcp --child needs --parent-socket and --parent-lane".into(),
        ));
    }
    Ok(parsed)
}

pub fn split_model(model: &str) -> Option<(String, String)> {
    let slash = model.find('/')?;
    (slash > 0 && slash < model.len() - 1).then(|| (model[..slash].to_string(), model[slash + 1..].to_string()))
}

/// The models and modes a server runs with (env, settings and flags resolved).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub frame_model: String,
    /// The frame model was named (`--frame-model`, ULTRON_CLAUDE_FRAME_MODEL, `claudeCode.frameModel`), not the
    /// default: it beats `rlm.frameModel` and ULTRON_RLM_FRAME_MODEL.
    pub frame_model_explicit: bool,
    pub children: ChildrenMode,
    pub child_model: String,
    /// The Claude children's model was named (`--child-model`, ULTRON_CLAUDE_CHILD_MODEL, `claudeCode.childModel`):
    /// `rlm.childModel` never overrides it.
    pub child_model_explicit: bool,
    pub ultron_child_model: Option<String>,
    /// The Claude Code model the root runs on, as named: `--root-model` (from the launcher), else a Claude Code
    /// subagent's server's `--child-model` (its root is that subagent). [`subscription_model`] makes it a model.
    pub root_model: Option<String>,
}

/// The Claude root's own subscription model as `claude-code/<id>`: the root's model when `known` (the model
/// registry has `claude-code/<id>`), else the default frame model ([`default_frame_model`]). Every Ultron child of a
/// Claude root that names no model runs on it ([`ultron_child_model`]): a `claude-code` request with tools runs on
/// the Claude subscription only (ai::claude_subscription::claude_code_lane), never on API credits.
pub fn subscription_model(root_model: Option<&str>, known: &dyn Fn(&str) -> bool) -> String {
    root_model
        .map(str::trim)
        .map(|model| crate::claude_code_id(model).unwrap_or(model))
        .filter(|id| !id.is_empty() && known(id))
        .map_or_else(default_frame_model, crate::claude_code_ref)
}

/// `rlm.childModel` as written when it names a Claude Code model; under `ultron claude` another provider's model in
/// that setting (a native session's choice) is not used.
pub fn claude_code_setting(model: Option<&str>) -> Option<String> {
    model
        .map(str::trim)
        .filter(|model| crate::claude_code_id(model).is_some_and(|id| !id.is_empty()))
        .map(str::to_string)
}

/// The model an Ultron child of a Claude root runs on when nothing it was started with names one (an in-memory
/// `--children ultron` subagent, a persistent child, a typed agent's `rlm` lane): `--ultron-child-model` /
/// `claudeCode.ultronChildModel`, else `rlm.childModel` when it names a Claude Code model ([`claude_code_setting`]),
/// else the root's subscription model ([`subscription_model`]). Never the settings' default model: TypeScript falls
/// back to it, but under a Claude root that put children on API credits the user never chose for them.
pub fn ultron_child_model(resolved: &Resolved, rlm_child_model: Option<&str>, subscription: &str) -> String {
    resolved
        .ultron_child_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
        .or_else(|| claude_code_setting(rlm_child_model))
        .unwrap_or_else(|| subscription.to_string())
}

/// `runMcpServer`'s model and mode resolution. An environment variable that is set wins even when blank (JS `??`).
pub fn resolve(args: &McpServerArgs, var: &dyn Fn(&str) -> Option<String>, cwd: &Path, agent_dir: &Path) -> Resolved {
    let settings = crate::settings::ClaudeCodeSettings::load(cwd, agent_dir);
    let trimmed = |key: &str| var(key).map(|v| v.trim().to_string());
    let named_frame_model = args
        .frame_model
        .clone()
        .or_else(|| trimmed(config::env::CLAUDE_FRAME_MODEL_ENV))
        .or(settings.frame_model.clone());
    let frame_model_explicit = named_frame_model.is_some();
    let frame_model = named_frame_model.unwrap_or_else(default_frame_model);
    let children = args
        .children
        .or_else(|| var(CLAUDE_CHILDREN_ENV).as_deref().and_then(ChildrenMode::parse))
        .or(settings.children)
        .unwrap_or(if resolve_claude_binary(var).is_none() {
            ChildrenMode::Ultron
        } else {
            ChildrenMode::Claude
        });
    let named_child_model = args
        .child_model
        .clone()
        .or_else(|| trimmed(config::env::CLAUDE_CHILD_MODEL_ENV))
        .or(settings.child_model.clone());
    let child_model_explicit = named_child_model.is_some();
    let child_model = named_child_model.unwrap_or_else(|| CLAUDE_CODE_DEFAULT_MODEL.into());
    let ultron_child_model = args.ultron_child_model.clone().or(settings.ultron_child_model);
    Resolved {
        frame_model,
        frame_model_explicit,
        children,
        child_model,
        child_model_explicit,
        ultron_child_model,
        root_model: args
            .root_model
            .clone()
            .or_else(|| args.child_model.clone().filter(|_| args.child)),
    }
}
