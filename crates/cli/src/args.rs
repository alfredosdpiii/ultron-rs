//! `parseArgs` from packages/coding-agent/src/cli/args.ts.

use serde_json::{Map, Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticKind {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    pub message: String,
}

/// A value of an unknown (possibly extension) flag: `--flag` alone is `True`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    True,
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListModels {
    All,
    Search(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub thinking: Option<String>,
    pub continue_: bool,
    pub resume: bool,
    /// `-r <name|id>`: the argument after `--resume`/`-r` when it is not a flag (a Rust addition; TypeScript's
    /// `--resume` takes no value).
    pub resume_selector: Option<String>,
    pub help: bool,
    pub version: bool,
    pub mode: Option<String>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Option<Vec<String>>,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: bool,
    pub themes: Option<Vec<String>>,
    pub use_theme: Option<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<ListModels>,
    pub offline: bool,
    pub claude: bool,
    pub tui_mode: Option<String>,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Insertion-ordered; a repeated flag keeps its first position and its last value, like a JS `Map`.
    pub unknown_flags: Vec<(String, FlagValue)>,
    pub diagnostics: Vec<Diagnostic>,
}

/// `normalizeSessionName`: the trimmed name, or `None` when blank.
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = jsrt::string::trim(value);
    (!name.is_empty()).then(|| name.to_string())
}

fn split_list(value: &str, drop_empty: bool) -> Vec<String> {
    value
        .split(',')
        .map(|s| jsrt::string::trim(s).to_string())
        .filter(|s| !drop_empty || !s.is_empty())
        .collect()
}

fn error(result: &mut Args, message: String) {
    result.diagnostics.push(Diagnostic {
        kind: DiagnosticKind::Error,
        message,
    });
}

fn set_flag(flags: &mut Vec<(String, FlagValue)>, name: &str, value: FlagValue) {
    match flags.iter_mut().find(|(k, _)| k == name) {
        Some(entry) => entry.1 = value,
        None => flags.push((name.to_string(), value)),
    }
}

type BoolFlagHandler = fn(&mut Args);
type ValueFlagHandler = fn(&mut Args, String);

const BOOL_FLAGS: &[(&[&str], BoolFlagHandler)] = &[
    (&["--help", "-h"], |r| r.help = true),
    (&["--version", "-v"], |r| r.version = true),
    (&["--continue", "-c"], |r| r.continue_ = true),
    (&["--no-session"], |r| r.no_session = true),
    (&["--no-tools", "-nt"], |r| r.no_tools = true),
    (&["--no-builtin-tools", "-nbt"], |r| r.no_builtin_tools = true),
    (&["--no-extensions", "-ne"], |r| r.no_extensions = true),
    (&["--no-skills", "-ns"], |r| r.no_skills = true),
    (&["--no-prompt-templates", "-np"], |r| r.no_prompt_templates = true),
    (&["--no-themes"], |r| r.no_themes = true),
    (&["--no-context-files", "-nc"], |r| r.no_context_files = true),
    (&["--verbose"], |r| r.verbose = true),
    (&["--approve", "-a"], |r| r.project_trust_override = Some(true)),
    (&["--no-approve", "-na"], |r| r.project_trust_override = Some(false)),
    (&["--offline"], |r| r.offline = true),
    (&["--claude"], |r| r.claude = true),
];

const VALUE_FLAGS: &[(&[&str], ValueFlagHandler)] = &[
    (&["--provider"], |r, v| r.provider = Some(v)),
    (&["--model"], |r, v| r.model = Some(v)),
    (&["--api-key"], |r, v| r.api_key = Some(v)),
    (&["--system-prompt"], |r, v| r.system_prompt = Some(v)),
    (&["--append-system-prompt"], |r, v| {
        push_value(&mut r.append_system_prompt, v);
    }),
    (&["--session"], |r, v| r.session = Some(v)),
    (&["--session-id"], |r, v| r.session_id = Some(v)),
    (&["--fork"], |r, v| r.fork = Some(v)),
    (&["--session-dir"], |r, v| r.session_dir = Some(v)),
    (&["--models"], |r, v| r.models = Some(split_list(&v, false))),
    (&["--tools", "-t"], |r, v| r.tools = Some(split_list(&v, true))),
    (&["--exclude-tools", "-xt"], |r, v| {
        r.exclude_tools = Some(split_list(&v, true));
    }),
    (&["--thinking"], set_thinking),
    (&["--export"], |r, v| r.export = Some(v)),
    (&["--extension", "-e"], |r, v| push_value(&mut r.extensions, v)),
    (&["--skill"], |r, v| push_value(&mut r.skills, v)),
    (&["--prompt-template"], |r, v| push_value(&mut r.prompt_templates, v)),
    (&["--theme"], |r, v| push_value(&mut r.themes, v)),
];

type SpecialFlagHandler = fn(&mut Args, &[String], &mut usize);

const SPECIAL_FLAGS: &[(&[&str], SpecialFlagHandler)] = &[
    (&["--mode"], parse_mode),
    (&["--resume", "-r"], parse_resume),
    (&["--name", "-n"], parse_name),
    (&["--print", "-p"], parse_print),
    (&["--use-theme"], parse_use_theme),
    (&["--list-models"], parse_list_models),
    (&["--tui-mode"], parse_tui_mode),
];

fn next_arg(args: &[String], i: usize) -> Option<&str> {
    args.get(i + 1).map(String::as_str)
}

fn push_value(list: &mut Option<Vec<String>>, value: String) {
    list.get_or_insert_with(Vec::new).push(value);
}

fn set_thinking(r: &mut Args, level: String) {
    if model_scope::THINKING_LEVELS.contains(&level.as_str()) {
        r.thinking = Some(level);
    } else {
        r.diagnostics.push(Diagnostic {
            kind: DiagnosticKind::Warning,
            message: format!(
                "Invalid thinking level \"{level}\". Valid values: {}",
                model_scope::THINKING_LEVELS.join(", ")
            ),
        });
    }
}

fn parse_mode(r: &mut Args, args: &[String], i: &mut usize) {
    match next_arg(args, *i) {
        None => error(r, "--mode requires text, json, or rpc".into()),
        Some(mode) if mode.starts_with('-') => error(r, "--mode requires text, json, or rpc".into()),
        Some(mode) => {
            *i += 1;
            if matches!(mode, "text" | "json" | "rpc") {
                r.mode = Some(mode.to_string());
            } else {
                error(r, format!("Invalid mode \"{mode}\". Valid values: text, json, rpc"));
            }
        }
    }
}

fn parse_resume(r: &mut Args, args: &[String], i: &mut usize) {
    r.resume = true;
    if let Some(n) = next_arg(args, *i)
        && !n.starts_with('-')
        && !n.starts_with('@')
    {
        r.resume_selector = Some(n.to_string());
        *i += 1;
    }
}

fn parse_name(r: &mut Args, args: &[String], i: &mut usize) {
    if let Some(value) = take(args, i) {
        r.name = Some(value);
    } else {
        *i -= 1;
        error(r, "--name requires a value".into());
    }
}

fn parse_print(r: &mut Args, args: &[String], i: &mut usize) {
    r.print = true;
    if let Some(n) = next_arg(args, *i)
        && !n.starts_with('@')
        && (!n.starts_with('-') || n.starts_with("---"))
    {
        r.messages.push(n.to_string());
        *i += 1;
    }
}

fn parse_use_theme(r: &mut Args, args: &[String], i: &mut usize) {
    match next_arg(args, *i) {
        Some(name) if !name.starts_with('-') => {
            r.use_theme = Some(name.to_string());
            *i += 1;
        }
        _ => error(r, "--use-theme requires a theme name".into()),
    }
}

fn parse_list_models(r: &mut Args, args: &[String], i: &mut usize) {
    match next_arg(args, *i) {
        Some(p) if !p.starts_with('-') && !p.starts_with('@') => {
            r.list_models = Some(ListModels::Search(p.to_string()));
            *i += 1;
        }
        _ => r.list_models = Some(ListModels::All),
    }
}

fn parse_tui_mode(r: &mut Args, args: &[String], i: &mut usize) {
    match next_arg(args, *i) {
        Some(mode @ ("regular" | "fullscreen")) => {
            r.tui_mode = Some(mode.to_string());
            *i += 1;
        }
        None => error(r, "--tui-mode requires regular or fullscreen".into()),
        Some(mode) if mode.starts_with('-') => error(r, "--tui-mode requires regular or fullscreen".into()),
        Some(mode) => {
            *i += 1;
            error(
                r,
                format!("Invalid TUI mode \"{mode}\". Valid values: regular, fullscreen"),
            );
        }
    }
}

fn parse_positional(r: &mut Args, positional: &str) {
    match positional.strip_prefix('@') {
        Some(file) => r.file_args.push(file.to_string()),
        None => r.messages.push(positional.to_string()),
    }
}

fn parse_unknown_or_positional(r: &mut Args, arg: &str, args: &[String], i: &mut usize) {
    if let Some(file) = arg.strip_prefix('@') {
        r.file_args.push(file.to_string());
        return;
    }
    if let Some(flag) = arg.strip_prefix("--") {
        parse_unknown_long_flag(r, flag, args, i);
        return;
    }
    if arg.starts_with('-') {
        error(r, format!("Unknown option: {arg}"));
    } else {
        r.messages.push(arg.to_string());
    }
}

fn parse_unknown_long_flag(r: &mut Args, flag: &str, args: &[String], i: &mut usize) {
    if let Some((name, value)) = flag.split_once('=') {
        set_flag(&mut r.unknown_flags, name, FlagValue::Text(value.to_string()));
        return;
    }
    match next_arg(args, *i) {
        Some(n) if !n.starts_with('-') && !n.starts_with('@') => {
            set_flag(&mut r.unknown_flags, flag, FlagValue::Text(n.to_string()));
            *i += 1;
        }
        _ => set_flag(&mut r.unknown_flags, flag, FlagValue::True),
    }
}

pub fn parse_args(args: &[String]) -> Args {
    let mut r = Args::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            for positional in &args[i + 1..] {
                parse_positional(&mut r, positional);
            }
            break;
        }
        parse_one_arg(&mut r, arg, args, &mut i);
        i += 1;
    }
    r
}

fn parse_one_arg(r: &mut Args, arg: &str, args: &[String], i: &mut usize) {
    if let Some((_, apply)) = BOOL_FLAGS.iter().find(|(names, _)| names.contains(&arg)) {
        apply(r);
        return;
    }
    if let Some((_, apply)) = SPECIAL_FLAGS.iter().find(|(names, _)| names.contains(&arg)) {
        apply(r, args, i);
        return;
    }
    if *i + 1 < args.len()
        && let Some((_, apply)) = VALUE_FLAGS.iter().find(|(names, _)| names.contains(&arg))
        && let Some(value) = take(args, i)
    {
        apply(r, value);
        return;
    }
    parse_unknown_or_positional(r, arg, args, i);
}

fn take(args: &[String], i: &mut usize) -> Option<String> {
    *i += 1;
    args.get(*i).cloned()
}

impl Args {
    /// The TypeScript `Args` object as `JSON.stringify` shows it (absent fields omitted), with `unknownFlags` as
    /// its entry list; used by the differential tests.
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut opt = |k: &str, v: Option<Value>| {
            if let Some(v) = v {
                m.insert(k.to_string(), v);
            }
        };
        let s = |v: &Option<String>| v.as_ref().map(|v| json!(v));
        let l = |v: &Option<Vec<String>>| v.as_ref().map(|v| json!(v));
        let b = |v: bool| v.then_some(json!(true));
        opt("provider", s(&self.provider));
        opt("model", s(&self.model));
        opt("apiKey", s(&self.api_key));
        opt("systemPrompt", s(&self.system_prompt));
        opt("appendSystemPrompt", l(&self.append_system_prompt));
        opt("thinking", s(&self.thinking));
        opt("continue", b(self.continue_));
        opt("resume", b(self.resume));
        opt("resumeSelector", s(&self.resume_selector));
        opt("help", b(self.help));
        opt("version", b(self.version));
        opt("mode", s(&self.mode));
        opt("name", s(&self.name));
        opt("noSession", b(self.no_session));
        opt("session", s(&self.session));
        opt("sessionId", s(&self.session_id));
        opt("fork", s(&self.fork));
        opt("sessionDir", s(&self.session_dir));
        opt("models", l(&self.models));
        opt("tools", l(&self.tools));
        opt("excludeTools", l(&self.exclude_tools));
        opt("noTools", b(self.no_tools));
        opt("noBuiltinTools", b(self.no_builtin_tools));
        opt("extensions", l(&self.extensions));
        opt("noExtensions", b(self.no_extensions));
        opt("print", b(self.print));
        opt("export", s(&self.export));
        opt("noSkills", b(self.no_skills));
        opt("skills", l(&self.skills));
        opt("promptTemplates", l(&self.prompt_templates));
        opt("noPromptTemplates", b(self.no_prompt_templates));
        opt("themes", l(&self.themes));
        opt("useTheme", s(&self.use_theme));
        opt("noThemes", b(self.no_themes));
        opt("noContextFiles", b(self.no_context_files));
        opt(
            "listModels",
            self.list_models.as_ref().map(|l| match l {
                ListModels::All => json!(true),
                ListModels::Search(p) => json!(p),
            }),
        );
        opt("offline", b(self.offline));
        opt("claude", b(self.claude));
        opt("tuiMode", s(&self.tui_mode));
        opt("verbose", b(self.verbose));
        opt("projectTrustOverride", self.project_trust_override.map(|v| json!(v)));
        m.insert("messages".into(), json!(self.messages));
        m.insert("fileArgs".into(), json!(self.file_args));
        let flags: Vec<Value> = self
            .unknown_flags
            .iter()
            .map(|(k, v)| match v {
                FlagValue::True => json!([k, true]),
                FlagValue::Text(t) => json!([k, t]),
            })
            .collect();
        m.insert("unknownFlags".into(), Value::Array(flags));
        let diagnostics: Vec<Value> = self
            .diagnostics
            .iter()
            .map(|d| {
                let kind = if d.kind == DiagnosticKind::Error {
                    "error"
                } else {
                    "warning"
                };
                json!({"type": kind, "message": d.message})
            })
            .collect();
        m.insert("diagnostics".into(), Value::Array(diagnostics));
        Value::Object(m)
    }
}
