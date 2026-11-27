//! The `ultron` command line: argument parsing (`cli/args.ts`), help, version, `--list-models` (`cli/list-models.ts`)
//! native session selection (`native-command.ts`) and prompted runs (`-p`, `--mode json`, see [`print_mode`]).

/// The `claude-subscription` provider's inert MCP inventory server (`ultron __internal inert-mcp`).
pub use ai::claude_subscription::inert_mcp;

pub mod acp;
pub mod args;
pub mod assets;
pub mod auth;
pub mod autoreview_ban;
pub mod autoreview_cli;
pub mod autoreview_toggle;
pub mod bug_report;
pub mod config_selector;
pub mod durable_client;
pub mod experimental;
pub mod export_html;
pub mod file_args;
pub mod gc_cli;
pub mod herdr_report;
pub mod instances;
pub mod interactive;
pub mod js;
pub mod judge_cli;
pub mod line_input;
pub mod link;
pub mod list_models;
pub mod mcp_cli;
pub mod mcp_login;
pub mod memory_cli;
pub mod memory_migrate;
pub mod migrate;
pub mod optimize_cli;
pub mod print_mode;
pub mod radius_relay;
pub mod rpc;
pub mod sandbox_cli;
pub mod schedule_cli;
pub mod semver;
pub mod service_cli;
pub mod session;
pub mod session_cli;
pub mod setup;
pub mod setup_core;
pub mod setup_tui;
pub mod setup_wizard;
pub mod skills_cli;
pub mod startup;
pub mod startup_checks;
pub mod terminal_settings;
pub mod themes;
pub mod tools_manager;
pub mod trace_cli;
pub mod tui_login;
pub mod update_cli;
pub mod usage_cli;
pub mod watch;

use args::{Args, DiagnosticKind, ListModels, normalize_session_name, parse_args};
use print_mode::Target;
use session_journal::catalog;
use session_journal::names::{self, SessionListing};
use session_journal::{SessionRepo, SystemClock};
use std::io::Write;
use std::path::Path;

pub const HELP: &str = include_str!("help.txt");

pub use session_core::VarLookup;

/// The process environment and locations one run sees (the session runtime's own).
pub type Env = session_core::RunEnv;

/// The instance a command uses (crates/daemon instance.rs): `ULTRON_SERVER_DIR`, else `durable`, else the project.
pub fn instance(env: &Env, durable: bool) -> daemon::instance::Instance {
    daemon::instance::resolve(&*env.var, &env.home, &env.cwd, durable)
}

/// Make the package files (`env.package`) ready (installed and held in use) for a run that reads them;
/// `--version`, `--help`, `--export` and the management subcommands never do (`cli::assets`).
pub fn package_files(env: &Env) -> Result<(), String> {
    assets::package_dir(&*env.var, &env.home).map(|_| ())
}

pub fn run(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    match run_inner(argv, env, out, err) {
        Ok(code) => code,
        Err(e) => {
            let _ = writeln!(err, "Error: {e}");
            1
        }
    }
}

/// A subcommand's answer for the whole command line: its exit code, or `None` when the line is not that command
/// after all (the line is then the next row's, or a prompt).
type Run = fn(&[String], &Env, &mut dyn Write, &mut dyn Write) -> Option<Result<i32, String>>;

/// A subcommand, in the order main.ts tries them before option parsing.
struct Command {
    /// The first arguments it answers; none: any line ([`experimental`], gated by `PI_EXPERIMENTAL`).
    names: &'static [&'static str],
    /// It reads the package files (`cli::assets`), made ready first.
    package_files: bool,
    /// The global `httpProxy` setting applies (`applyHttpProxySettings`): to this command and every one after it.
    proxy: bool,
    /// TypeScript handles the name before option parsing: a line this build does not answer is not implemented
    /// rather than a prompt.
    typescript: bool,
    run: Run,
}

const COMMANDS: &[Command] = &[
    Command {
        names: &[],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |argv, env, out, err| experimental::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["instances"],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |_, env, out, _| Some(instances::list(env, out)),
    },
    Command {
        names: &["durable"],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |argv, env, out, err| Some(instances::durable(&argv[1..], env, out, err)),
    },
    Command {
        names: &["trace"],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |argv, _, out, err| Some(Ok(trace_cli::run(&argv[1..], out, err))),
    },
    Command {
        names: &["autoreview"],
        package_files: true,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| autoreview_cli::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["auth"],
        package_files: false,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| auth::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["setup"],
        package_files: false,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| setup::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["migrate"],
        package_files: false,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| migrate::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["usage"],
        package_files: false,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| usage_cli::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["mcp"],
        package_files: false,
        proxy: false,
        typescript: true,
        run: |argv, env, out, err| mcp_cli::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["install", "remove", "uninstall", "update", "list"],
        package_files: false,
        proxy: true,
        typescript: true,
        run: |argv, env, out, err| update_cli::run_update(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["config"],
        package_files: false,
        proxy: true,
        typescript: true,
        run: |argv, env, out, err| update_cli::run_config(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["claude", "agy", "mcp", "hook", "guide"],
        package_files: true,
        proxy: true,
        typescript: true,
        run: |argv, env, out, err| claude_root::cli::run(argv, env, out, err).map(Ok),
    },
    Command {
        names: &["watch"],
        package_files: true,
        proxy: true,
        typescript: true,
        run: |argv, env, out, err| {
            let result = watch::run(&argv[1..], &env.cwd.display().to_string(), &*env.var, out);
            Some(Ok(claude_root::cli::exit_code(result, err)))
        },
    },
    Command {
        names: &["memory"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| {
            matches!(argv.get(1).map(String::as_str), Some("import" | "migrate"))
                .then(|| memory_cli::run(&argv[1..], env, out, err))
        },
    },
    Command {
        names: &["schedule"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| schedule_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["service"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| service_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["session"],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |argv, env, out, err| session_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["judge"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| judge_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["optimize"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| optimize_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["skills"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| skills_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["sandbox"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| Some(sandbox_cli::run(&argv[1..], env, out, err)),
    },
    Command {
        names: &["gc"],
        package_files: false,
        proxy: false,
        typescript: false,
        run: |argv, env, out, err| Some(gc_cli::run(&argv[1..], env, out, err)),
    },
];

fn io_err(e: std::io::Error) -> String {
    e.to_string()
}

fn dispatch_subcommand(
    argv: &[String],
    env: &mut Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<i32>, String> {
    let mut proxied = false;
    let first = argv.first().map(String::as_str);
    for command in COMMANDS {
        if command.proxy && !proxied {
            *env = startup::proxy_env(env);
            proxied = true;
        }
        if !command.names.is_empty() && !first.is_some_and(|name| command.names.contains(&name)) {
            continue;
        }
        if command.package_files {
            package_files(env)?;
        }
        if let Some(result) = (command.run)(argv, env, out, err) {
            return result.map(Some);
        }
    }
    Ok(None)
}

fn report_diagnostics_and_warnings(parsed: &Args, err: &mut dyn Write) -> Result<bool, String> {
    for d in &parsed.diagnostics {
        let label = if d.kind == DiagnosticKind::Error {
            "Error"
        } else {
            "Warning"
        };
        writeln!(err, "{label}: {}", d.message).map_err(io_err)?;
    }
    if parsed.diagnostics.iter().any(|d| d.kind == DiagnosticKind::Error) {
        return Ok(true);
    }
    for warning in startup::extension_flag_warnings(parsed)
        .into_iter()
        .chain(startup::ignored_flag_warnings(parsed))
    {
        writeln!(err, "Warning: {warning}").map_err(io_err)?;
    }
    Ok(false)
}

fn handle_export(
    input: &str,
    output: Option<&str>,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, String> {
    match export_html::export(input, output, env) {
        Ok(path) => {
            writeln!(out, "Exported to: {path}").map_err(io_err)?;
            Ok(0)
        }
        Err(message) => {
            writeln!(err, "Error: {message}").map_err(io_err)?;
            Ok(1)
        }
    }
}

fn handle_list_models(list: &ListModels, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let search = match list {
        ListModels::All => None,
        ListModels::Search(p) => Some(p.as_str()),
    };
    if let Some(warning) = list_models::models_json_warning(env) {
        err.write_all(warning.as_bytes()).map_err(io_err)?;
    }
    package_files(env)?;
    let models = session_core::lane::available_models(env);
    out.write_all(list_models::render(&models, search, &env.docs()).as_bytes())
        .map_err(io_err)?;
    Ok(0)
}

fn handle_early_flags(
    parsed: &Args,
    env: &Env,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Option<i32>, String> {
    if parsed.version {
        writeln!(out, "{}", config::version()).map_err(io_err)?;
        return Ok(Some(0));
    }
    if let Some(input) = &parsed.export {
        let output = parsed.messages.first().map(String::as_str);
        return handle_export(input, output, env, out, err).map(Some);
    }
    if parsed.help {
        out.write_all(HELP.as_bytes()).map_err(io_err)?;
        return Ok(Some(0));
    }
    if let Some(list) = &parsed.list_models {
        return handle_list_models(list, env, out, err).map(Some);
    }
    Ok(None)
}

fn apply_claude_root(parsed: &mut Args, env: &Env, err: &mut dyn Write) -> Result<Result<Option<Env>, i32>, String> {
    let claude_setting = claude_root::settings::ClaudeCodeSettings::load(&env.cwd, &env.agent_dir()).root;
    if !parsed.claude && !session_core::core::claude_root_requested(&*env.var) && !claude_setting {
        return Ok(Ok(None));
    }
    match claude_root::worker_root::subscription_root(parsed.provider.as_deref(), parsed.model.as_deref(), &*env.var) {
        Ok(selection) => {
            parsed.model = Some(selection.model);
            parsed.provider = selection.provider;
            let rooted = startup::overlay(
                env,
                vec![(session_core::core::ROOT_ENV.to_string(), "claude".to_string())],
            );
            Ok(Ok(Some(rooted)))
        }
        Err(message) => {
            writeln!(err, "Error: {message}").map_err(io_err)?;
            Ok(Err(1))
        }
    }
}

fn run_acp(parsed: Args, env: &Env, err: &mut dyn Write) -> Result<i32, String> {
    if !parsed.file_args.is_empty() || !parsed.messages.is_empty() || parsed.mode.is_some() {
        writeln!(
            err,
            "Error: `ultron acp` takes session options only (no messages, @files or --mode)"
        )
        .map_err(io_err)?;
        return Ok(1);
    }
    if let Some(selector) = acp::session_selector(&parsed) {
        writeln!(
            err,
            "Error: `ultron acp` opens the sessions its client asks for (session/new, session/load): {selector} is not supported"
        )
        .map_err(io_err)?;
        return Ok(1);
    }
    acp::run(parsed, env, Box::new(std::io::stdin()), Box::new(std::io::stdout()))
}

fn print_tool_warnings(parsed: &Args, env: &Env, err: &mut dyn Write) -> Result<(), String> {
    for warning in startup::tool_flag_warnings(parsed)
        .into_iter()
        .chain(startup::tool_env_warnings(env))
    {
        writeln!(err, "Warning: {warning}").map_err(io_err)?;
    }
    Ok(())
}

fn report_native_result(result: Result<i32, NativeError>, env: &Env, err: &mut dyn Write) -> Result<i32, String> {
    match result {
        Ok(code) => Ok(code),
        Err(NativeError::Exit(message)) => {
            writeln!(err, "Error: {message}").map_err(io_err)?;
            Ok(1)
        }
        Err(NativeError::Failed(message)) => {
            writeln!(err, "Error: {message}").map_err(io_err)?;
            if let Some(hint) = setup::no_model_hint(env) {
                writeln!(err, "{hint}").map_err(io_err)?;
            }
            Ok(1)
        }
    }
}

fn run_native_with_warnings(parsed: &Args, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let rpc_mode = parsed.mode.as_deref() == Some("rpc");
    if rpc_mode && !parsed.file_args.is_empty() {
        writeln!(err, "Error: @file arguments are not supported in RPC mode").map_err(io_err)?;
        return Ok(1);
    }
    if !rpc_mode && !setup::offer_setup_on_first_run(parsed, env, out) {
        return Ok(0);
    }
    let stdin = startup::read_piped_stdin(parsed);
    print_tool_warnings(parsed, env, err)?;
    report_native_result(native_run(parsed, env, stdin, out, err), env, err)
}

fn check_unimplemented_ts(first: Option<&str>) -> Result<(), String> {
    if let Some(name) = first.filter(|name| COMMANDS.iter().any(|c| c.typescript && c.names.contains(name))) {
        return Err(format!("`ultron {name}` is not implemented in the Rust build yet"));
    }
    Ok(())
}

fn run_parsed(mut parsed: Args, acp: bool, env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    if let Some(code) = handle_early_flags(&parsed, env, out, err)? {
        return Ok(code);
    }
    package_files(env)?;
    print_mode::trace_startup(env, "cli.native");
    let rooted = match apply_claude_root(&mut parsed, env, err)? {
        Ok(rooted) => rooted,
        Err(code) => return Ok(code),
    };
    let env = rooted.as_ref().unwrap_or(env);
    if acp {
        return run_acp(parsed, env, err);
    }
    run_native_with_warnings(&parsed, env, out, err)
}

fn run_inner(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let mut env = startup::offline_env(argv, env);
    if let Some(code) = dispatch_subcommand(argv, &mut env, out, err)? {
        return Ok(code);
    }
    let first = argv.first().map(String::as_str);
    check_unimplemented_ts(first)?;
    let acp = first == Some("acp");
    let parsed = parse_args(if acp { &argv[1..] } else { argv });
    if report_diagnostics_and_warnings(&parsed, err)? {
        return Ok(1);
    }
    run_parsed(parsed, acp, &env, out, err)
}

/// How `runNativeUltronCommand` ends when it fails.
enum NativeError {
    /// `process.exit(1)` after its own message.
    Exit(String),
    /// A thrown error: reported with the setup hint.
    Failed(String),
}

impl From<String> for NativeError {
    fn from(message: String) -> Self {
        NativeError::Failed(message)
    }
}

fn native_io(e: std::io::Error) -> NativeError {
    NativeError::Failed(e.to_string())
}

fn assemble_prompt(parsed: &Args, env: &Env, stdin: Option<String>) -> Result<Option<print_mode::Prompt>, NativeError> {
    let files = if parsed.file_args.is_empty() {
        file_args::ProcessedFiles::default()
    } else {
        file_args::process_file_arguments(&parsed.file_args, &env.cwd, &env.home).map_err(NativeError::Exit)?
    };
    let parts: Vec<String> = stdin
        .into_iter()
        .chain(std::iter::once(files.text))
        .chain(parsed.messages.iter().cloned())
        .filter(|part| !part.is_empty())
        .collect();
    Ok((!parts.is_empty()).then(|| print_mode::Prompt {
        text: parts.join("\n\n"),
        images: files.images,
    }))
}

fn resolve_selection(
    selection: Selection,
    runs_tui: bool,
    env: &Env,
    out: &mut dyn Write,
) -> Result<Option<Target>, NativeError> {
    match selection {
        Selection::List(sessions) if runs_tui => {
            let stdin = std::io::stdin();
            let target = match interactive::pick_session(&sessions, &mut std::io::stdout(), &mut stdin.lock()) {
                Some(metadata) => Target::Open(metadata),
                None => Target::New { id: None },
            };
            Ok(Some(target))
        }
        Selection::List(sessions) => {
            out.write_all(render_session_list(&sessions, &env.cwd.display().to_string()).as_bytes())
                .map_err(native_io)?;
            Ok(None)
        }
        Selection::Target(target) => Ok(Some(target)),
    }
}

fn streaming_listener() -> client::runtime::EventListener {
    std::sync::Arc::new(|event: &serde_json::Value| {
        if event["type"] == "message_update"
            && event["frame"]["type"] == "text_delta"
            && let Some(delta) = event["frame"]["delta"].as_str()
        {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(delta.as_bytes());
            let _ = stdout.flush();
        }
    })
}

fn run_prompted(
    parsed: &Args,
    env: &Env,
    prompt: print_mode::Prompt,
    target: Target,
    json_mode: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, NativeError> {
    let streaming = !parsed.print && !json_mode;
    let on_event = streaming.then(streaming_listener);
    let herdr = herdr_report::PrintReport::start(env);
    let prompted = print_mode::run_prompt(parsed, env, prompt, target, on_event);
    if let Some(herdr) = &herdr {
        herdr.finish(prompted.as_ref().map(|p| p.text.as_str()).map_err(String::as_str));
    }
    let prompted = prompted?;
    for notice in &prompted.notices {
        writeln!(err, "{notice}").map_err(native_io)?;
    }
    let text = if streaming {
        "\n".to_string()
    } else {
        print_mode::render(json_mode, &prompted)
    };
    out.write_all(text.as_bytes()).map_err(native_io)?;
    Ok(0)
}

fn has_prompt_input(parsed: &Args, stdin: &Option<String>) -> bool {
    stdin.is_some() || !parsed.file_args.is_empty() || !parsed.messages.is_empty()
}

fn dispatch_native_target(
    parsed: &Args,
    env: &Env,
    target: Target,
    prompt: Option<print_mode::Prompt>,
    runs_tui: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, NativeError> {
    if parsed.mode.as_deref() == Some("rpc") {
        return Ok(rpc::remote::run(
            parsed,
            env,
            target,
            Box::new(std::io::stdin()),
            Box::new(std::io::stdout()),
        )?);
    }
    if runs_tui {
        return Ok(interactive::run_interactive(parsed, env, target)?);
    }
    let Some(prompt) = prompt else {
        out.write_all(print_mode::run_unprompted(parsed, env, target)?.as_bytes())
            .map_err(native_io)?;
        return Ok(0);
    };
    let json_mode = parsed.mode.as_deref() == Some("json");
    run_prompted(parsed, env, prompt, target, json_mode, out, err)
}

/// `runNativeUltronCommand`: select the session, assemble the prompt (piped stdin, `@file`s, messages), then run
/// RPC mode, the TUI (no prompt, not `-p`, on a terminal), or the client: a prompt's answer (`-p` prints it,
/// `--mode json` prints the result, otherwise the text streams as it arrives), or without a prompt the attached
/// session or the server's sessions.
fn native_run(
    parsed: &Args,
    env: &Env,
    stdin: Option<String>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, NativeError> {
    let rpc_mode = parsed.mode.as_deref() == Some("rpc");
    let terminal = interactive::is_terminal();
    let prompted = rpc_mode || has_prompt_input(parsed, &stdin);
    let selection = native_command(parsed, env, prompted, terminal)?;
    let prompt = assemble_prompt(parsed, env, stdin)?;
    let runs_tui = !rpc_mode && prompt.is_none() && !parsed.print && terminal;
    let Some(target) = resolve_selection(selection, runs_tui, env, out)? else {
        return Ok(0);
    };
    dispatch_native_target(parsed, env, target, prompt, runs_tui, out, err)
}

/// What the session flags select.
enum Selection {
    /// `ultron -r` without a selector or prompt: the sessions of this directory.
    List(Vec<SessionListing>),
    Target(Target),
}

/// The listing `ultron -r` prints: one line per session of the cwd, newest first, as
/// `<id>\t<modified ISO>\t<name>\t<first message (60 chars)>`.
pub fn render_session_list(sessions: &[SessionListing], cwd: &str) -> String {
    if sessions.is_empty() {
        return format!("No sessions in {cwd}\n");
    }
    let mut out = String::new();
    for session in sessions {
        let preview: String = session
            .first_message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(60)
            .collect();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            session.metadata.id,
            jsrt::date::iso_string(session.metadata.modified_at as i64),
            session.name.as_deref().unwrap_or(""),
            preview
        ));
    }
    out
}

fn validate_fork_flags(parsed: &Args) -> Result<(), String> {
    if parsed.fork.is_none() {
        return Ok(());
    }
    if parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.session_id.is_some() {
        return Err("--fork cannot be combined with another session selector".into());
    }
    if parsed.no_session {
        return Err("--fork cannot be combined with --no-session".into());
    }
    Ok(())
}

fn validate_native_args(parsed: &Args) -> Result<(), String> {
    validate_fork_flags(parsed)?;
    if let Some(dir) = &parsed.session_dir
        && !Path::new(dir).is_absolute()
    {
        return Err("--session-dir must be an absolute path for the native ultron command".into());
    }
    if let Some(name) = &parsed.name
        && normalize_session_name(name).is_none()
    {
        return Err("--name requires a non-empty value".into());
    }
    Ok(())
}

fn open_or_new_session(session_dir: &Path, id: String) -> Result<Selection, String> {
    Ok(Selection::Target(match catalog::find(session_dir, &id)? {
        Some(metadata) => Target::Open(metadata),
        None => Target::New { id: Some(id) },
    }))
}

fn select_resume(
    parsed: &Args,
    repo: &SessionRepo,
    cwd: &str,
    prompted: bool,
    terminal: bool,
) -> Result<Selection, String> {
    let listed = names::list_sessions(Path::new(repo.root()), cwd);
    if let Some(selector) = &parsed.resume_selector {
        let found = names::resolve_resume(&listed, selector)?;
        return Ok(Selection::Target(Target::Open(found.metadata.clone())));
    }
    if !prompted {
        return Ok(Selection::List(listed));
    }
    let picked = if terminal {
        let stdin = std::io::stdin();
        interactive::pick_session(&listed, &mut std::io::stdout(), &mut stdin.lock())
    } else {
        listed.into_iter().next().map(|newest| newest.metadata)
    };
    Ok(Selection::Target(match picked {
        Some(metadata) => Target::Open(metadata),
        None => Target::New { id: None },
    }))
}

fn select_continue(
    sessions: &[session_journal::Metadata],
    session_dir: &Path,
    repo: &SessionRepo,
) -> Result<Selection, String> {
    if let Some(newest) = sessions.first() {
        return open_or_new_session(session_dir, newest.id.clone());
    }
    let all = names::list_all_sessions(repo);
    Ok(Selection::Target(match all.into_iter().next() {
        Some(newest) => Target::Open(newest.metadata),
        None => Target::New { id: None },
    }))
}

fn load_catalog_sessions(parsed: &Args, env: &Env, session_dir: &Path) -> Vec<session_journal::Metadata> {
    let needs_sessions = parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.fork.is_some();
    if !needs_sessions {
        return Vec::new();
    }
    let filter = (!durable_client::is_client(env)).then(|| env.cwd.display().to_string());
    catalog::list(session_dir, filter.as_deref())
}

fn select_fork(
    fork: &str,
    sessions: &[session_journal::Metadata],
    session_dir: &Path,
    cwd: &Path,
) -> Result<Selection, String> {
    let id = session::session_id_from_selector(fork, sessions, cwd)?;
    let metadata = catalog::find(session_dir, &id)?.ok_or_else(|| format!("Unknown session: {id}"))?;
    Ok(Selection::Target(Target::Fork(metadata)))
}

fn select_default_session(
    parsed: &Args,
    sessions: &[session_journal::Metadata],
    session_dir: &Path,
    repo: &SessionRepo,
    cwd: &Path,
) -> Result<Selection, String> {
    if parsed.continue_ {
        return select_continue(sessions, session_dir, repo);
    }
    if let Some(selector) = &parsed.session {
        let id = session::session_id_from_selector(selector, sessions, cwd)?;
        return open_or_new_session(session_dir, id);
    }
    Ok(Selection::Target(if parsed.no_session {
        Target::Ephemeral
    } else {
        Target::New { id: None }
    }))
}

/// The checks `runNativeUltronCommand` makes before it starts a server, and its session selection.
fn native_command(parsed: &Args, env: &Env, prompted: bool, terminal: bool) -> Result<Selection, String> {
    validate_native_args(parsed)?;
    let session_dir = session::session_directory(parsed.session_dir.as_deref(), env);
    print_mode::trace_startup(env, "cli.list-sessions");
    let repo = SessionRepo::new(&session_dir, SystemClock::shared());
    let sessions = load_catalog_sessions(parsed, env, &session_dir);
    if let Some(fork) = &parsed.fork {
        return select_fork(fork, &sessions, &session_dir, &env.cwd);
    }
    if let Some(id) = &parsed.session_id {
        return open_or_new_session(&session_dir, id.clone());
    }
    if parsed.resume {
        let cwd = env.cwd.display().to_string();
        return select_resume(parsed, &repo, &cwd, prompted, terminal);
    }
    select_default_session(parsed, &sessions, &session_dir, &repo, &env.cwd)
}
