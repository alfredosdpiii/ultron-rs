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
pub mod herdr_report;
pub mod instances;
pub mod interactive;
pub mod js;
pub mod line_input;
pub mod link;
pub mod list_models;
pub mod mcp_cli;
pub mod mcp_login;
pub mod memory_cli;
pub mod migrate;
pub mod print_mode;
pub mod radius_relay;
pub mod rpc;
pub mod sandbox_cli;
pub mod schedule_cli;
pub mod semver;
pub mod service_cli;
pub mod session;
pub mod setup;
pub mod setup_core;
pub mod setup_tui;
pub mod setup_wizard;
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

/// An environment variable lookup usable from any thread.
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

/// Run the CLI; returns the exit code.
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
    // `runExperimentalCommand` (PI_EXPERIMENTAL=1): `ultron server` and `ultron client`.
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
    // `ultron trace check <file>`: the resets of a request trace (`ULTRON_REQUEST_TRACE`).
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
    // `ultron mcp add|remove|list|login|logout`; bare `ultron mcp` is Ultron's own MCP server (below).
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
        names: &["claude", "mcp", "hook", "guide"],
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
    // `ultron memory import ...` only: a bare `ultron memory` stays a prompt, as in TypeScript.
    Command {
        names: &["memory"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| {
            (argv.get(1).map(String::as_str) == Some("import")).then(|| memory_cli::run(&argv[1..], env, out, err))
        },
    },
    // `ultron schedule list|wake|cancel <id>|--help` only: any other `ultron schedule ...` stays a prompt.
    Command {
        names: &["schedule"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| schedule_cli::run(&argv[1..], env, out, err),
    },
    // `ultron service install|uninstall|status|restart|run|--help` only: any other `ultron service ...` stays a
    // prompt.
    Command {
        names: &["service"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| service_cli::run(&argv[1..], env, out, err),
    },
    Command {
        names: &["sandbox"],
        package_files: false,
        proxy: true,
        typescript: false,
        run: |argv, env, out, err| Some(sandbox_cli::run(&argv[1..], env, out, err)),
    },
];

fn run_inner(argv: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Result<i32, String> {
    let io = |e: std::io::Error| e.to_string();
    // main.ts: `--offline` (or PI_OFFLINE) holds for every command, the subcommands included.
    let mut env = startup::offline_env(argv, env);
    let mut proxied = false;
    let first = argv.first().map(String::as_str);
    for command in COMMANDS {
        if command.proxy && !proxied {
            env = startup::proxy_env(&env);
            proxied = true;
        }
        if !command.names.is_empty() && !first.is_some_and(|name| command.names.contains(&name)) {
            continue;
        }
        if command.package_files {
            package_files(&env)?;
        }
        if let Some(result) = (command.run)(argv, &env, out, err) {
            return result;
        }
    }
    let env = &env;
    if let Some(name) = first.filter(|name| COMMANDS.iter().any(|c| c.typescript && c.names.contains(name))) {
        return Err(format!("`ultron {name}` is not implemented in the Rust build yet"));
    }
    // `ultron acp [options]`: an Agent Client Protocol agent on stdio (crate::acp); the options are the session's.
    let acp = argv.first().is_some_and(|a| a == "acp");
    let mut parsed = parse_args(if acp { &argv[1..] } else { argv });
    for d in &parsed.diagnostics {
        let label = if d.kind == DiagnosticKind::Error {
            "Error"
        } else {
            "Warning"
        };
        writeln!(err, "{label}: {}", d.message).map_err(io)?;
    }
    if parsed.diagnostics.iter().any(|d| d.kind == DiagnosticKind::Error) {
        return Ok(1);
    }
    for warning in startup::extension_flag_warnings(&parsed)
        .into_iter()
        .chain(startup::ignored_flag_warnings(&parsed))
    {
        writeln!(err, "Warning: {warning}").map_err(io)?;
    }
    if parsed.version {
        writeln!(out, "{}", config::version()).map_err(io)?;
        return Ok(0);
    }
    if let Some(input) = &parsed.export {
        let output = parsed.messages.first().map(String::as_str);
        return match export_html::export(input, output, env) {
            Ok(path) => {
                writeln!(out, "Exported to: {path}").map_err(io)?;
                Ok(0)
            }
            Err(message) => {
                writeln!(err, "Error: {message}").map_err(io)?;
                Ok(1)
            }
        };
    }
    if parsed.help {
        out.write_all(HELP.as_bytes()).map_err(io)?;
        return Ok(0);
    }
    if let Some(list) = &parsed.list_models {
        let search = match list {
            ListModels::All => None,
            ListModels::Search(p) => Some(p.as_str()),
        };
        if let Some(warning) = list_models::models_json_warning(env) {
            err.write_all(warning.as_bytes()).map_err(io)?;
        }
        package_files(env)?;
        let models = list_models::available_models(env);
        out.write_all(list_models::render(&models, search, &env.docs()).as_bytes())
            .map_err(io)?;
        return Ok(0);
    }
    // Everything below runs a session (or reads its files): the package files must exist from here on.
    package_files(env)?;
    if let Some(note) = startup::package_settings_note(env) {
        writeln!(err, "Note: {note}").map_err(io)?;
    }
    if let Some(note) = startup::extension_settings_note(env) {
        writeln!(err, "Note: {note}").map_err(io)?;
    }
    // The native worker command starts here (main.ts `shouldRunUltronWorker`).
    print_mode::trace_startup(env, "cli.native");
    // `--claude`: the root lane runs on Claude, through the claude-subscription provider (worker-root.ts
    // `prepareClaudeRoot`'s selection; crates/claude-root `subscription_root`).
    let claude_setting = claude_root::settings::ClaudeCodeSettings::load(&env.cwd, &env.agent_dir()).root;
    let mut rooted = None;
    if parsed.claude || session_core::core::claude_root_requested(&*env.var) || claude_setting {
        match claude_root::worker_root::subscription_root(
            parsed.provider.as_deref(),
            parsed.model.as_deref(),
            &*env.var,
        ) {
            Ok(selection) => {
                parsed.model = Some(selection.model);
                parsed.provider = selection.provider;
                // `ULTRON_ROOT=claude` for the session workers (TypeScript sets it in `process.env`): a resumed
                // session's lane moves to the root model there (session-worker.ts).
                rooted = Some(startup::overlay(
                    env,
                    vec![(session_core::core::ROOT_ENV.to_string(), "claude".to_string())],
                ));
            }
            Err(message) => {
                writeln!(err, "Error: {message}").map_err(io)?;
                return Ok(1);
            }
        }
    }
    let env = rooted.as_ref().unwrap_or(env);
    if acp {
        if !parsed.file_args.is_empty() || !parsed.messages.is_empty() || parsed.mode.is_some() {
            writeln!(
                err,
                "Error: `ultron acp` takes session options only (no messages, @files or --mode)"
            )
            .map_err(io)?;
            return Ok(1);
        }
        if let Some(selector) = acp::session_selector(&parsed) {
            writeln!(
                err,
                "Error: `ultron acp` opens the sessions its client asks for (session/new, session/load): {selector} is not supported"
            )
            .map_err(io)?;
            return Ok(1);
        }
        return acp::run(parsed, env, Box::new(std::io::stdin()), Box::new(std::io::stdout()));
    }
    let rpc_mode = parsed.mode.as_deref() == Some("rpc");
    if rpc_mode && !parsed.file_args.is_empty() {
        writeln!(err, "Error: @file arguments are not supported in RPC mode").map_err(io)?;
        return Ok(1);
    }
    // An interactive start with no usable model offers the guided setup; other modes are never prompted.
    if !rpc_mode && !setup::offer_setup_on_first_run(&parsed, env, out) {
        return Ok(0);
    }
    // RPC reads its commands from stdin, so it is not read as a prompt.
    let stdin = startup::read_piped_stdin(&parsed);
    for warning in startup::tool_flag_warnings(&parsed)
        .into_iter()
        .chain(startup::tool_env_warnings(env))
    {
        writeln!(err, "Warning: {warning}").map_err(io)?;
    }
    match native_run(&parsed, env, stdin, out, err) {
        Ok(code) => Ok(code),
        Err(NativeError::Exit(message)) => {
            // processFileArguments reports and exits on its own (no setup hint).
            writeln!(err, "Error: {message}").map_err(io)?;
            Ok(1)
        }
        Err(NativeError::Failed(message)) => {
            writeln!(err, "Error: {message}").map_err(io)?;
            if let Some(hint) = startup::no_model_hint(env) {
                writeln!(err, "{hint}").map_err(io)?;
            }
            Ok(1)
        }
    }
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
    let io = |e: std::io::Error| NativeError::Failed(e.to_string());
    let json_mode = parsed.mode.as_deref() == Some("json");
    let rpc_mode = parsed.mode.as_deref() == Some("rpc");
    let terminal = interactive::is_terminal();
    let has_prompt = stdin.is_some() || !parsed.file_args.is_empty() || !parsed.messages.is_empty();
    let selection = native_command(parsed, env, rpc_mode || has_prompt, terminal)?;
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
    let prompt = (!parts.is_empty()).then(|| print_mode::Prompt {
        text: parts.join("\n\n"),
        images: files.images,
    });
    let runs_tui = !rpc_mode && prompt.is_none() && !parsed.print && terminal;
    let target = match selection {
        // On a terminal, `ultron -r` picks one of this directory's sessions and opens it in the TUI.
        Selection::List(sessions) if runs_tui => {
            let stdin = std::io::stdin();
            match interactive::pick_session(&sessions, &mut std::io::stdout(), &mut stdin.lock()) {
                Some(metadata) => Target::Open(metadata),
                None => Target::New { id: None },
            }
        }
        Selection::List(sessions) => {
            out.write_all(render_session_list(&sessions, &env.cwd.display().to_string()).as_bytes())
                .map_err(io)?;
            return Ok(0);
        }
        Selection::Target(target) => target,
    };
    if rpc_mode {
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
            .map_err(io)?;
        return Ok(0);
    };
    // Neither `-p` nor `--mode json`: the answer streams to stdout as it arrives (`onEvent` text deltas).
    let streaming = !parsed.print && !json_mode;
    let on_event: Option<client::runtime::EventListener> = streaming.then(|| {
        std::sync::Arc::new(|event: &serde_json::Value| {
            if event["type"] == "message_update"
                && event["frame"]["type"] == "text_delta"
                && let Some(delta) = event["frame"]["delta"].as_str()
            {
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(delta.as_bytes());
                let _ = stdout.flush();
            }
        }) as client::runtime::EventListener
    });
    // herdr's agents panel: `working` for the run, then its end's state, released when this returns.
    let herdr = herdr_report::PrintReport::start(env);
    let prompted = print_mode::run_prompt(parsed, env, prompt, target, on_event);
    if let Some(herdr) = &herdr {
        herdr.finish(prompted.as_ref().map(|p| p.text.as_str()).map_err(String::as_str));
    }
    let prompted = prompted?;
    // A Claude turn that switched to API credits (or back) says so, beside the answer.
    for notice in &prompted.notices {
        writeln!(err, "{notice}").map_err(io)?;
    }
    let text = if streaming {
        "\n".to_string()
    } else {
        print_mode::render(json_mode, &prompted)
    };
    out.write_all(text.as_bytes()).map_err(io)?;
    Ok(0)
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

/// The checks `runNativeUltronCommand` makes before it starts a server, and its session selection.
fn native_command(parsed: &Args, env: &Env, prompted: bool, terminal: bool) -> Result<Selection, String> {
    if parsed.fork.is_some()
        && (parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.session_id.is_some())
    {
        return Err("--fork cannot be combined with another session selector".into());
    }
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
    let session_dir = session::session_directory(parsed.session_dir.as_deref(), env);
    print_mode::trace_startup(env, "cli.list-sessions");
    let repo = SessionRepo::new(&session_dir, SystemClock::shared());
    let needs_sessions = parsed.continue_ || parsed.resume || parsed.session.is_some() || parsed.fork.is_some();
    // A client of the durable instance selects among all of the instance's sessions, wherever they were started
    // (`ultron-s` runs its sessions in the home directory; herdr resumes them as `ultron durable --session <id>`).
    let durable = durable_client::is_client(env);
    let sessions = match (needs_sessions, durable) {
        (false, _) => Vec::new(),
        (true, false) => catalog::list(&session_dir, Some(&env.cwd.display().to_string())),
        (true, true) => catalog::list(&session_dir, None),
    };
    if parsed.fork.is_some() && parsed.no_session {
        return Err("--fork cannot be combined with --no-session".into());
    }
    let cwd = env.cwd.display().to_string();
    let open_or_new = |id: String| -> Result<Selection, String> {
        Ok(Selection::Target(match catalog::find(&session_dir, &id)? {
            Some(metadata) => Target::Open(metadata),
            None => Target::New { id: Some(id) },
        }))
    };
    if let Some(fork) = &parsed.fork {
        let id = session::session_id_from_selector(fork, &sessions, &env.cwd)?;
        let metadata = catalog::find(&session_dir, &id)?.ok_or_else(|| format!("Unknown session: {id}"))?;
        return Ok(Selection::Target(Target::Fork(metadata)));
    }
    if let Some(id) = &parsed.session_id {
        return open_or_new(id.clone());
    }
    if parsed.resume {
        let listed = names::list_sessions(Path::new(repo.root()), &cwd);
        if let Some(selector) = &parsed.resume_selector {
            let found = names::resolve_resume(&listed, selector)?;
            return Ok(Selection::Target(Target::Open(found.metadata.clone())));
        }
        if !prompted {
            return Ok(Selection::List(listed));
        }
        // `selectedSessionId`: on a terminal the user picks one (`selectInteractiveSession`); otherwise the newest.
        let picked = if terminal {
            let stdin = std::io::stdin();
            interactive::pick_session(&listed, &mut std::io::stdout(), &mut stdin.lock())
        } else {
            listed.into_iter().next().map(|newest| newest.metadata)
        };
        return Ok(Selection::Target(match picked {
            Some(metadata) => Target::Open(metadata),
            None => Target::New { id: None },
        }));
    }
    if parsed.continue_ {
        // The newest format-4 session of this directory (a durable client's: of the instance, as `ultron-s -c`),
        // else the newest session the server lists (any cwd).
        if let Some(newest) = sessions.first() {
            return open_or_new(newest.id.clone());
        }
        let all = names::list_all_sessions(&repo);
        return Ok(Selection::Target(match all.into_iter().next() {
            Some(newest) => Target::Open(newest.metadata),
            None => Target::New { id: None },
        }));
    }
    if let Some(selector) = &parsed.session {
        return open_or_new(session::session_id_from_selector(selector, &sessions, &env.cwd)?);
    }
    Ok(Selection::Target(if parsed.no_session {
        Target::Ephemeral
    } else {
        Target::New { id: None }
    }))
}
