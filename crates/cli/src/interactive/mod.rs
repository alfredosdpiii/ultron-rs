//! `ultron` with no prompt on a terminal: the interactive TUI (crates/interactive) as a client of the session it
//! shows, like TypeScript's client TUI (experimental/client-tui.ts): this process's server generation, the session
//! attached through a [`Link`] (the session runs in its worker: lane, host stack, format-4 journal, names), its
//! transcript's lane-watch events turned into the TUI's [`LaneEvent`]s, and every TUI command a session operation.
//!
//! The UI thread owns the app and the terminal and never waits on the session: the app's [`LaneCommand`]s go through
//! [`route`] to a link thread, which owns the [`Link`] and answers through UI events.

mod link;
mod link_handlers;
mod support;

pub use support::{apply_presentation_plugins, import_pi, login_options, pi_agent_dir, pi_sessions};

use crate::Env;
use crate::args::Args;
use crate::print_mode::Target;
use client::session::SessionLink as Link;
use interactive::app::{App, AppOptions};
use interactive::history::{PROMPT_HISTORY_FILE, PromptHistoryStore};
use interactive::keybindings::native_app_keybindings;
use interactive::lane::{LaneCommand, LaneEvent, ModelInfo, SessionOpened};
use interactive::run::{UiEvent, run_stdout, spawn_gated_input};
use link::{Cancels, Command, link_thread};
use protocol::watch::WatchEvent;
use serde_json::{Value, json};
use session_core::core::Op;
use std::io::{BufRead, IsTerminal, Write};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use support::{catalog, catalog_info, model_info, start_call};
use tui::KeybindingsManager;
use tui::UserBindings;

pub const PI_AGENT_DIR_ENV: &str = "PI_CODING_AGENT_DIR";
pub const SPLASH_ENV: &str = "ULTRON_SPLASH";

/// The tree's nodes with `checkpoint: true` on the entries a checkpoint (with a commit) is attached to.
pub fn mark_checkpoints(mut tree: Value, checkpoints: &Value) -> Value {
    let marked: std::collections::HashSet<&str> = checkpoints["checkpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["commit"].is_string())
        .filter_map(|c| c["entryId"].as_str())
        .collect();
    if marked.is_empty() {
        return tree;
    }
    let mut stack: Vec<&mut Value> = tree.as_array_mut().into_iter().flatten().collect();
    while let Some(node) = stack.pop() {
        if node["entry"]["id"].as_str().is_some_and(|id| marked.contains(id)) {
            node["checkpoint"] = json!(true);
        }
        if let Some(children) = node.get_mut("children").and_then(Value::as_array_mut) {
            stack.extend(children.iter_mut());
        }
    }
    tree
}

fn spawn_startup_changelog(env: Env, ui: mpsc::Sender<UiEvent>) {
    std::thread::spawn(move || {
        if let Some(notice) = crate::startup_checks::startup_changelog(&env) {
            let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
        }
    });
}

fn spawn_subscription_auth_warning(env: Env, ui: mpsc::Sender<UiEvent>, provider: String) {
    std::thread::spawn(move || {
        if crate::startup_checks::subscription_auth_warns(&env, &provider) {
            let notice = interactive::lane::StartupNotice::SubscriptionAuth;
            let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
        }
    });
}

fn start_oauth_login(
    env: &Env,
    ui: &mpsc::Sender<UiEvent>,
    commands: &mpsc::Sender<Command>,
    login: &Arc<crate::tui_login::LoginSlot>,
    login_id: u64,
    provider: String,
    name: String,
) {
    if !crate::tui_login::supported(env, &provider) {
        let _ = ui.send(UiEvent::Lane(LaneEvent::Status(format!(
            "Error: Failed to login to {name}: signing in with an account is not available in the Rust build yet; use an API key"
        ))));
        return;
    }
    let event_ui = ui.clone();
    let events: Arc<dyn Fn(LaneEvent) + Send + Sync> = Arc::new(move |event| {
        let _ = event_ui.send(UiEvent::Lane(event));
    });
    let (saved_commands, id, label) = (commands.clone(), provider.clone(), name.clone());
    let saved = Box::new(move || {
        let _ = saved_commands.send(Command::LoginSaved(id, label));
    });
    crate::tui_login::start(env.clone(), login_id, provider, name, events, login.clone(), saved);
}

fn spawn_abort_op(link: &client::session::LinkHandle, ui: mpsc::Sender<UiEvent>, op: Op) {
    let started = start_call(
        &link.connection(),
        protocol::names::SESSION_OPS,
        "op",
        json!([op.to_json()]),
    );
    std::thread::spawn(move || {
        let result = started.and_then(|(connection, request)| connection.wait(&request).map_err(client::lost::message));
        if let Err(error) = result {
            let _ = ui.send(UiEvent::Lane(LaneEvent::Error(error)));
        }
    });
}

fn route_control_command(
    command: LaneCommand,
    link: &client::session::LinkHandle,
    ui: &mpsc::Sender<UiEvent>,
    cancels: &Cancels,
) -> Option<LaneCommand> {
    match command {
        LaneCommand::Abort => spawn_abort_op(link, ui.clone(), Op::Abort),
        LaneCommand::AbortRetry => spawn_abort_op(link, ui.clone(), Op::AbortRetry),
        LaneCommand::AbortBash => spawn_abort_op(link, ui.clone(), Op::AbortBash),
        LaneCommand::CancelShare => jsrt::sync::lock(&cancels.share).abort("cancelled"),
        LaneCommand::CancelBugReport => cancels.bug.store(true, std::sync::atomic::Ordering::SeqCst),
        other => return Some(other),
    }
    None
}

fn route_command(
    command: LaneCommand,
    link: &client::session::LinkHandle,
    commands: &mpsc::Sender<Command>,
    env: &Env,
    ui: &mpsc::Sender<UiEvent>,
    cancels: &Cancels,
    login: &Arc<crate::tui_login::LoginSlot>,
) {
    let Some(command) = route_control_command(command, link, ui, cancels) else {
        return;
    };
    match command {
        LaneCommand::StartupChangelog => spawn_startup_changelog(env.clone(), ui.clone()),
        LaneCommand::SubscriptionAuthWarning(provider) => {
            spawn_subscription_auth_warning(env.clone(), ui.clone(), provider);
        }
        LaneCommand::OAuthLogin { id, provider, name } => {
            start_oauth_login(env, ui, commands, login, id, provider, name);
        }
        LaneCommand::LoginReply { id, answer } => login.reply(id, answer),
        command => {
            if matches!(command, LaneCommand::Share { .. }) {
                *jsrt::sync::lock(&cancels.share) = kernel::Signal::new();
            }
            let _ = commands.send(Command::Lane(command));
        }
    }
}

/// The TUI's commands, in order: the ones that must not wait behind the link thread's current command (an abort, a
/// cancel, a login's answer, a startup check) are answered here; the rest go to the link thread.
fn route(
    link: client::session::LinkHandle,
    lane: mpsc::Receiver<LaneCommand>,
    commands: mpsc::Sender<Command>,
    env: Env,
    ui: mpsc::Sender<UiEvent>,
    cancels: Cancels,
) {
    let login: Arc<crate::tui_login::LoginSlot> = Arc::default();
    for command in lane {
        route_command(command, &link, &commands, &env, &ui, &cancels, &login);
    }
}

/// The streaming assistant message, folded from `message_update` frames (protocol::watch::stream_fold): an update carries
/// its frame, not the message so far, so the TUI's events ([`interactive::lane_watch::lane_events`]) get the message
/// this folds.
#[derive(Default)]
pub struct TuiStream {
    message: Option<Value>,
}

impl TuiStream {
    /// The TUI events of a lane-watch event, each frame-only update folded into the started message first.
    pub fn events(&mut self, value: &Value) -> Vec<LaneEvent> {
        let Some(mut event) = WatchEvent::decode(value) else {
            return Vec::new();
        };
        match &mut event {
            WatchEvent::MessageStart { message, .. } if message["role"] == "assistant" => {
                self.message = Some(message.clone());
            }
            WatchEvent::MessageUpdate {
                message: shown @ None, ..
            } => {
                let Some(message) = self.message.as_mut() else {
                    return Vec::new();
                };
                protocol::watch::stream_fold::fold_update(message, value);
                *shown = Some(message.clone());
            }
            WatchEvent::MessageEnd { message, .. } if message["role"] == "assistant" => self.message = None,
            WatchEvent::RunEnd { .. } => self.message = None,
            _ => {}
        }
        interactive::lane_watch::lane_events(&event)
    }
}

/// The session the link is attached to, as the TUI shows it after it opened (with its thinking level).
pub(super) fn opened(link: &Link) -> Result<(SessionOpened, ModelInfo, Vec<Value>, Option<String>), String> {
    let described = link.op(&Op::Describe)?.unwrap_or_default();
    Ok((
        SessionOpened {
            session_id: described["sessionId"].as_str().unwrap_or_default().to_string(),
            session_file: described["sessionFile"].as_str().map(str::to_string),
            name: described["name"].as_str().map(str::to_string),
            messages: described["messages"].as_array().cloned().unwrap_or_default(),
        },
        model_info(&described["model"]),
        described["entries"].as_array().cloned().unwrap_or_default(),
        described["thinkingLevel"].as_str().map(str::to_string),
    ))
}

/// `selectInteractiveSession` for `ultron -r`: this directory's sessions as a numbered list (with their names), read
/// a number from stdin. One session is taken without asking.
pub fn pick_session(
    sessions: &[session_journal::names::SessionListing],
    out: &mut dyn Write,
    input: &mut dyn BufRead,
) -> Option<session_journal::repo::Metadata> {
    match sessions {
        [] => return None,
        [only] => return Some(only.metadata.clone()),
        _ => {}
    }
    let _ = writeln!(out, "Available sessions:");
    for (i, s) in sessions.iter().enumerate() {
        let name = s.name.as_deref().map_or_default(|n| format!("  {n}"));
        let _ = writeln!(out, "  {}. {}{name}", i + 1, s.metadata.id);
    }
    let _ = write!(out, "Select a session number: ");
    let _ = out.flush();
    let mut answer = String::new();
    input.read_line(&mut answer).ok()?;
    let index = answer.trim().parse::<usize>().ok()?.checked_sub(1)?;
    sessions.get(index).map(|s| s.metadata.clone())
}

pub fn is_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// A second unexpected worker stop within this long is reported instead of recovered (`WORKER_RECOVERY_INTERVAL_MS`).
pub(super) const WORKER_RECOVERY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

const SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the interactive TUI on `target`'s session until the user exits.
pub fn run_interactive(parsed: &Args, env: &Env, target: Target) -> Result<i32, String> {
    if matches!(target, Target::New { .. } | Target::Ephemeral) {
        crate::print_mode::resolve_model(parsed, env)?;
    }
    let (open_args, open_env) = (parsed.clone(), env.clone());
    run_tui(
        parsed,
        env,
        Box::new(move |watch| crate::link::open(&open_args, &open_env, &target, "interactive", watch)),
    )
}

/// The client TUI (`runClientTui`) on a server this client reaches but does not run (`ultron client --connect`):
/// the session `selection` names on it. A Radius route's connection is recovered while the TUI waits, with its
/// statuses (`Radius disconnected; retrying…`, `Reconnecting to Radius…`, `Reattaching Session…`); a Unix route's
/// loss is reported.
pub fn run_remote_interactive(
    parsed: &Args,
    env: &Env,
    route: client::session::RemoteRoute,
    selection: client::session::RemoteSelection,
) -> Result<i32, String> {
    run_tui(
        parsed,
        env,
        Box::new(move |watch| Link::open_remote(route, &selection, watch)),
    )
}

/// Whether a link watch event says the session's worker restarted under it (crate::link `needs_rejoin`): the link
/// re-attaches by itself; the TUI shows the session again. A Radius route recovers through its own events.
pub fn is_worker_restart(event: &Value) -> bool {
    event["type"] == "connection"
        && event["state"] == "reconnecting"
        && event["message"] == client::session::WORKER_RESTARTED
}

fn radius_connection_state(event: &Value) -> client::radius::ConnectionState {
    match event["state"].as_str() {
        Some("connecting") => client::radius::ConnectionState::Connecting,
        Some("connected") => client::radius::ConnectionState::Connected,
        _ => client::radius::ConnectionState::Disconnected,
    }
}

fn radius_attachment_state(event: &Value) -> (client::radius::AttachmentState, String) {
    let id = event["sessionId"].as_str().unwrap_or_default().to_string();
    let state = if event["state"] == "attached" {
        client::radius::AttachmentState::Attached(id.clone())
    } else {
        client::radius::AttachmentState::Attaching(id.clone())
    };
    (state, id)
}

/// What a remote link's recovery events show (client-tui.ts `#handleConnectionState`, `#handleAttachmentState`,
/// `#handleLocalConnectionState`), given whether the lane is open: the TUI events, and whether the session is to be
/// shown again (the lane reopened after a reattachment).
pub fn recovery_events(event: &Value, lane_open: &mut bool) -> (Vec<LaneEvent>, bool) {
    use client::radius::{attachment_state_view, connection_state_view};
    let view = match event["type"].as_str() {
        Some("radius_connection") => connection_state_view(radius_connection_state(event), *lane_open),
        Some("radius_attachment") => {
            let (state, id) = radius_attachment_state(event);
            attachment_state_view(&state, &id)
        }
        Some("connection") if event["state"] == "lost" => {
            let message = event["message"].as_str().unwrap_or_default();
            return (
                vec![LaneEvent::Recovery {
                    busy: false,
                    status: format!("Error: {message}"),
                }],
                false,
            );
        }
        _ => return (Vec::new(), false),
    };
    let Some(view) = view else {
        return (Vec::new(), false);
    };
    let reopen = event["type"] == "radius_attachment" && !view.busy && !*lane_open;
    if view.close_lane {
        *lane_open = false;
    }
    if !view.busy {
        *lane_open = true;
    }
    (
        vec![LaneEvent::Recovery {
            busy: view.busy,
            status: view.status.to_string(),
        }],
        reopen,
    )
}

/// `KeybindingsManager.create(agentDir)` (core/keybindings.ts): the user bindings in `<agentDir>/keybindings.json`,
/// legacy names migrated and ordered by the TypeScript definitions; nothing when the file is missing or malformed.
pub fn user_keybindings(agent_dir: &std::path::Path, platform: interactive::keybindings::Platform) -> UserBindings {
    tui::load_user_bindings(
        &tui::keybindings_path(agent_dir),
        &interactive::keybindings::app_keybindings(platform),
    )
}

/// The TUI's debug outputs and Termux flag from `env` ([`interactive::run::terminal_debug`]): the redraw log goes to
/// the agent directory (`logDirectory: agentDir`), dumps to `dump_dir`; the write-log file is named by local time.
pub fn terminal_debug(env: &Env, dump_dir: &std::path::Path) -> (Option<std::sync::Arc<tui::RenderDebug>>, bool) {
    let now = jsrt::date::now_ms();
    let zone = jsrt::localtime::Zone::from_env((env.var)("TZ").as_deref());
    let fields = zone.fields(now as f64);
    let second = (now.div_euclid(1000) + zone.offset(now as f64)).rem_euclid(60) as u32;
    let local = tui::LocalStamp {
        year: fields.year,
        month: fields.month,
        day: fields.day,
        hour: fields.hour,
        minute: fields.minute,
        second,
    };
    interactive::run::terminal_debug(
        &|name| (env.var)(name),
        &env.agent_dir(),
        dump_dir,
        local,
        std::process::id(),
        Box::new(jsrt::date::now_ms),
    )
}

/// The settings the TUI presents itself by, with this run's flags over them: `--verbose` shows the startup splash
/// whatever `quietStartup` says, `--tui-mode regular|fullscreen` is this run's `tuiMode`.
pub fn presentation_settings(parsed: &Args, settings: &serde_json::Map<String, Value>) -> Value {
    let mut settings = settings.clone();
    if parsed.verbose {
        settings.insert("quietStartup".into(), Value::Bool(false));
    }
    if let Some(mode) = &parsed.tui_mode {
        settings.insert("tuiMode".into(), Value::from(mode.as_str()));
    }
    Value::Object(settings)
}

fn make_watch_sink(
    ui: mpsc::Sender<UiEvent>,
    commands: mpsc::Sender<Command>,
    herdr: Option<Arc<crate::herdr_report::TuiReport>>,
) -> client::session::WatchSink {
    let lane_open = std::sync::Mutex::new(true);
    let stream = std::sync::Mutex::new(TuiStream::default());
    Arc::new(move |event: Value| {
        if let Some(herdr) = &herdr {
            herdr.lane_event(&event);
        }
        if event["type"] == "run_end" {
            let _ = commands.send(Command::RefreshName);
        }
        if is_worker_restart(&event) {
            let _ = commands.send(Command::WorkerStopped);
        }
        let (recovery, reopen) = recovery_events(&event, &mut jsrt::sync::lock(&lane_open));
        let shown = jsrt::sync::lock(&stream).events(&event);
        for lane_event in shown.into_iter().chain(recovery) {
            let _ = ui.send(UiEvent::Lane(lane_event));
        }
        if reopen {
            let _ = commands.send(Command::Reopen);
        }
    })
}

fn resolve_external_editor(settings: &config::settings::SettingsManager, env: &Env) -> Option<String> {
    settings
        .settings()
        .get("externalEditor")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .map(str::to_string)
        .or_else(|| (env.var)("VISUAL").filter(|v| !v.is_empty()))
        .or_else(|| (env.var)("EDITOR").filter(|v| !v.is_empty()))
        .or_else(|| Some("nano".to_string()))
}

fn spawn_version_check(env: Env, ui: mpsc::Sender<UiEvent>) {
    std::thread::spawn(move || {
        if let Some(version) = crate::startup_checks::check_for_new_version(&env, config::version()) {
            let notice = interactive::lane::StartupNotice::NewVersion(version);
            let _ = ui.send(UiEvent::Lane(LaneEvent::StartupNotice(notice)));
        }
    });
}

fn configure_app(
    app: &mut App,
    parsed: &Args,
    env: &Env,
    settings: &config::settings::SettingsManager,
    presentation: &Value,
    caps: &tui::Capabilities,
    ui_tx: &mpsc::Sender<UiEvent>,
) {
    let theme_setting = parsed.use_theme.clone().or_else(|| settings.theme());
    let hide_thinking = settings.settings().get("hideThinkingBlock") == Some(&Value::Bool(true));
    let enabled_models: Vec<String> = parsed
        .models
        .clone()
        .unwrap_or_else(|| config::keys::enabled_models(settings.settings()));
    app.set_hide_thinking(hide_thinking);
    app.set_enabled_models(enabled_models);
    let wezterm = (env.var)("WEZTERM_PANE").is_some_and(|v| !v.is_empty())
        || (env.var)("TERM_PROGRAM").is_some_and(|v| v.eq_ignore_ascii_case("wezterm"));
    app.set_image_protocol(caps.images.filter(|p| *p == tui::ImageProtocol::Kitty), wezterm);
    let mut registry = theme::registry::ThemeRegistry::new(
        config::paths::AgentPaths::new(env.agent_dir()).custom_themes_dir(),
        tui::color_mode(caps),
    );
    let _ = registry.set_registered_themes(crate::themes::load(env, registry.mode).themes);
    app.set_theme_controller(interactive::theme_setup::ThemeController::new(
        registry,
        theme_setting,
        (env.var)("COLORFGBG").as_deref(),
    ));
    app.apply_startup_settings(presentation, &|k| (env.var)(k));
    let tree_filter = settings
        .settings()
        .get("treeFilterMode")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_string();
    let skip_summary = settings.branch_summary_settings()["skipPrompt"] == true;
    app.set_tree_settings(&tree_filter, skip_summary);
    app.set_auto_compact(settings.compaction_enabled() != false);
    app.set_external_editor(resolve_external_editor(settings, env));
    spawn_version_check(env.clone(), ui_tx.clone());
    let wake = ui_tx.clone();
    app.set_autocomplete_waker(move || {
        let _ = wake.send(UiEvent::Wake);
    });
    let (debug, termux) = terminal_debug(env, std::path::Path::new(tui::RENDER_DUMP_DIR));
    app.set_terminal_debug(debug, termux);
}

fn build_app_options(
    parsed: &Args,
    env: &Env,
    qol: interactive::qol::QolSettings,
    theme: Rc<theme::Theme>,
    server_id: String,
) -> AppOptions {
    let model = crate::print_mode::resolve_model(parsed, env)
        .ok()
        .map_or_default(|m| catalog_info(&m));
    let splash =
        !(env.var)(SPLASH_ENV).is_some_and(|v| matches!(v.trim().to_lowercase().as_str(), "off" | "0" | "false"));
    let platform = crate::terminal_settings::platform(env);
    let user_keys = user_keybindings(&env.agent_dir(), platform);
    let key_context = tui::KeyContext::from_env(&|k| (env.var)(k));
    let keys = KeybindingsManager::new(native_app_keybindings(platform), user_keys, key_context);
    let highlight = interactive::rlm_highlight::rlm_highlighter(&theme);
    AppOptions {
        theme,
        keys,
        version: config::version().to_string(),
        cwd: env.cwd.display().to_string(),
        home: Some(env.home.display().to_string()).filter(|h| !h.is_empty()),
        server_id,
        models: catalog(env),
        model,
        thinking_level: parsed.thinking.clone().unwrap_or_else(|| "medium".into()),
        splash,
        history: PromptHistoryStore::new(
            Some(env.agent_dir().join(PROMPT_HISTORY_FILE)),
            &env.cwd.display().to_string(),
        ),
        fd_path: crate::tools_manager::ensure_fd(env, crate::tools_manager::GITHUB),
        highlight,
        qol,
        bug_report_target: (
            crate::bug_report::gateway_host(&crate::bug_report::gateway_url(env)),
            config::env::offline(&*env.var),
        ),
    }
}

/// Run the TUI over the link `open_link` opens (its watch events go to the TUI) until the user exits.
fn run_tui(
    parsed: &Args,
    env: &Env,
    open_link: Box<dyn FnOnce(client::session::WatchSink) -> Result<Link, String>>,
) -> Result<i32, String> {
    let settings = config::settings::SettingsManager::for_worker(&env.cwd, &env.agent_dir(), &env.home);
    let presentation = presentation_settings(parsed, settings.settings());
    let caps = crate::terminal_settings::capabilities(env, &settings);
    let theme_setting = parsed.use_theme.clone().or_else(|| settings.theme());
    let theme = Rc::new(crate::themes::resolve(
        theme_setting.as_deref(),
        (env.var)("COLORFGBG").as_deref(),
        &config::paths::AgentPaths::new(env.agent_dir()).custom_themes_dir(),
        tui::color_mode(&caps),
    ));
    let qol = interactive::qol::QolSettings::from_settings(&presentation, &|k| (env.var)(k));
    interactive::render_settings::set(interactive::render_settings::RenderSettings {
        hyperlinks: caps.hyperlinks,
        code_block_indent: Some(config::keys::code_block_indent(settings.settings())),
        plain_links: qol.plain_links,
    });

    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let herdr = crate::herdr_report::TuiReport::start(parsed, env).map(Arc::new);
    let watch = make_watch_sink(ui_tx.clone(), command_tx.clone(), herdr.clone());
    let link = open_link(watch)?;
    let handle = link.handle();
    let server_id = link.connection().server_id.clone();
    let signalled = Arc::new(std::sync::atomic::AtomicI32::new(0));
    let cancels = Cancels::default();
    let (link_alive, link_ended) = mpsc::channel::<()>();
    {
        let ui = ui_tx.clone();
        let signalled = signalled.clone();
        let catalog_env = env.clone();
        let reporter = crate::bug_report::Reporter::new(cancels.bug.clone(), parsed.api_key.is_some());
        let (herdr, cancels, no_session) = (herdr.clone(), cancels.clone(), parsed.no_session);
        std::thread::spawn(move || {
            let _alive = link_alive;
            link_thread(
                link,
                command_rx,
                ui,
                no_session,
                signalled,
                catalog_env,
                reporter,
                herdr,
                cancels,
            );
        });
    }
    let (lane_tx, lane_rx) = mpsc::channel::<LaneCommand>();
    let router = {
        let (commands, env, ui) = (command_tx.clone(), env.clone(), ui_tx.clone());
        std::thread::spawn(move || route(handle, lane_rx, commands, env, ui, cancels))
    };
    let options = build_app_options(parsed, env, qol, theme, server_id);
    let mut app = App::new(options, lane_tx);
    let (code, end) = (signalled, std::sync::Mutex::new(ui_tx.clone()));
    crate::print_mode::exit_on_signal(move |exit| {
        code.store(exit, std::sync::atomic::Ordering::SeqCst);
        let _ = jsrt::sync::lock(&end).send(UiEvent::InputClosed);
    });
    configure_app(&mut app, parsed, env, &settings, &presentation, &caps, &ui_tx);
    spawn_gated_input(ui_tx, app.input_gate());
    let result = run_stdout(&mut app, &ui_rx, &tui::size);
    drop(app);
    let _ = router.join();
    let _ = command_tx.send(Command::Shutdown);
    let _ = link_ended.recv_timeout(SHUTDOWN_WAIT);
    if let Some(herdr) = &herdr {
        herdr.release();
    }
    result.map_err(|e| e.to_string())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use interactive::lane::Queued;
    use protocol::watch::QueueKind;

    /// The TUI events of one lane-watch event, as the watch shows them.
    fn tui_events(event: &Value) -> Vec<LaneEvent> {
        TuiStream::default().events(event)
    }

    fn listing(id: &str, name: Option<&str>) -> session_journal::names::SessionListing {
        session_journal::names::SessionListing {
            metadata: session_journal::repo::Metadata {
                id: id.into(),
                created_at: 0,
                storage_version: 1,
                cwd: "/c".into(),
                path: format!("/s/{id}.jsonl").into(),
                modified_at: 0.0,
                parent_session_id: None,
                legacy_parent_session_path: None,
            },
            name: name.map(str::to_string),
            automatic_name: false,
            first_message: "hi".into(),
            all_messages_text: "hi".into(),
            message_count: 1,
        }
    }

    /// A server directory too deep for a Unix socket address keeps its sockets in a private runtime directory;
    /// `/debug` finds the socket there, where the server put it.
    #[test]
    fn cli_debug_finds_the_socket_of_a_deep_server_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let deep = tmp.path().join("d".repeat(120));
        let server_dir = deep.display().to_string();
        let env = Env {
            cwd: tmp.path().to_path_buf(),
            home: tmp.path().to_path_buf(),
            var: std::sync::Arc::new(move |k: &str| (k == config::paths::ENV_SERVER_DIR).then(|| server_dir.clone())),
            package: tmp.path().to_path_buf(),
        };
        let socket = daemon::instance::socket_path(&deep, "srv1");
        assert!(!socket.starts_with(&deep), "{}", socket.display());
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        std::fs::write(&socket, "").unwrap();
        let found = support::local_server_socket(&env, "srv1");
        let _ = std::fs::remove_dir_all(socket.parent().unwrap());
        assert_eq!(found, Some(socket));
    }

    #[test]
    fn cli_resume_picker_lists_names_and_reads_a_number() {
        let sessions = [listing("a1", Some("Fix the parser")), listing("b2", None)];
        let mut out = Vec::new();
        let picked = pick_session(&sessions, &mut out, &mut "2\n".as_bytes());
        assert_eq!(picked.map(|m| m.id).as_deref(), Some("b2"));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Available sessions:\n  1. a1  Fix the parser\n  2. b2\nSelect a session number: "
        );
        assert_eq!(pick_session(&sessions, &mut Vec::new(), &mut "x\n".as_bytes()), None);
        assert_eq!(
            pick_session(&sessions[..1], &mut Vec::new(), &mut "".as_bytes())
                .map(|m| m.id)
                .as_deref(),
            Some("a1")
        );
    }

    #[test]
    fn cli_interactive_run_retry_events_become_tui_events() {
        assert_eq!(
            tui_events(
                &json!({"type": "retry_scheduled", "lane": "main", "attempt": 2, "maxAttempts": 3,
                "delayMs": 4000, "errorMessage": "Overloaded"})
            ),
            [LaneEvent::AutoRetryStart {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 4000
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "retry_end", "success": true, "attempt": 2})),
            [LaneEvent::AutoRetryEnd {
                attempt: 2,
                failed: None
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "retry_end", "success": false, "attempt": 3, "finalError": "Overloaded"})),
            [LaneEvent::AutoRetryEnd {
                attempt: 3,
                failed: Some("Overloaded".into())
            }]
        );
    }

    #[test]
    fn cli_interactive_summarization_retry_events_become_tui_events() {
        assert_eq!(
            tui_events(
                &json!({"type": "summarization_retry_scheduled", "lane": "main", "attempt": 1, "maxAttempts": 3,
                "delayMs": 2000, "errorMessage": "Overloaded"})
            ),
            [LaneEvent::SummarizationRetryScheduled {
                attempt: 1,
                max_attempts: 3,
                delay_ms: 2000,
                error: "Overloaded".into()
            }]
        );
        assert_eq!(
            tui_events(
                &json!({"type": "summarization_retry_attempt_start", "source": "compaction", "reason": "threshold"})
            ),
            [LaneEvent::SummarizationRetryStarted {
                branch_summary: false,
                reason: "threshold".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "summarization_retry_attempt_start", "source": "branchSummary"})),
            [LaneEvent::SummarizationRetryStarted {
                branch_summary: true,
                reason: "manual".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "summarization_retry_finished", "lane": "main"})),
            [LaneEvent::SummarizationRetryFinished]
        );
    }

    #[test]
    fn cli_interactive_compaction_and_thinking_events_become_tui_events() {
        assert_eq!(
            tui_events(&json!({"type": "compaction_start", "reason": "manual"})),
            [LaneEvent::CompactionStarted {
                reason: "manual".into()
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "compaction_start", "reason": "overflow"})),
            [LaneEvent::CompactionStarted {
                reason: "overflow".into()
            }]
        );
        assert_eq!(
            tui_events(
                &json!({"type": "compaction_end", "reason": "threshold", "status": "failed", "error": {"message": "Auto-compaction failed: boom"}})
            ),
            [LaneEvent::CompactionEnded {
                status: "failed".into(),
                error: Some("Auto-compaction failed: boom".into())
            }]
        );
        assert_eq!(
            tui_events(&json!({"type": "compaction_end", "reason": "threshold", "status": "declined"})),
            [LaneEvent::CompactionEnded {
                status: "declined".into(),
                error: None
            }]
        );
        assert!(
            tui_events(&json!({"type": "compaction_end", "reason": "threshold", "status": "completed"})).is_empty()
        );
        assert!(tui_events(&json!({"type": "compaction_end", "reason": "manual", "status": "failed"})).is_empty());
        let entry = json!({"type": "compaction", "id": "c1", "tokensBefore": 15, "retainedTail": []});
        assert_eq!(
            tui_events(&json!({"type": "entry_added", "entry": entry})),
            [LaneEvent::Compacted(Ok(entry))]
        );
        assert!(tui_events(&json!({"type": "entry_added", "entry": {"type": "message"}})).is_empty());
        assert_eq!(
            tui_events(&json!({"type": "config_update", "property": "thinkingLevel", "value": "high"})),
            [LaneEvent::Thinking("high".into())]
        );
        assert!(tui_events(&json!({"type": "config_update", "property": "model", "value": {}})).is_empty());
    }

    /// An update carries its frame, not the message so far: the TUI's stream folds it into the started message
    /// (protocol::watch::stream_fold). An older worker's update, whole, still shows as it is.
    #[test]
    fn cli_interactive_stream_folds_update_frames() {
        let mut stream = TuiStream::default();
        let start = json!({"type": "message_start", "runId": "r1", "lane": "main",
            "message": {"role": "assistant", "content": [], "stopReason": "pending"}});
        let update = |frame: Value| {
            json!({"type": "message_update", "runId": "r1", "lane": "main", "frame": frame,
            "usage": {"output": 1}})
        };
        assert_eq!(
            stream.events(&update(json!({"type": "text_start", "contentIndex": 0}))),
            []
        );
        stream.events(&start);
        stream.events(&update(json!({"type": "text_start", "contentIndex": 0})));
        let shown = stream.events(&update(json!({"type": "text_delta", "contentIndex": 0, "delta": "Hi"})));
        let [LaneEvent::Streaming(message)] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert_eq!(message["content"], json!([{"type": "text", "text": "Hi"}]));
        assert_eq!(message["usage"], json!({"output": 1}));
        stream.events(&json!({"type": "message_end", "message": {
            "role": "custom", "customType": "ultron.observer", "content": "You should know", "display": true
        }}));
        let shown = stream.events(&update(
            json!({"type": "text_delta", "contentIndex": 0, "delta": " there"}),
        ));
        let [LaneEvent::Streaming(message)] = shown.as_slice() else {
            panic!("{shown:?}");
        };
        assert_eq!(message["content"], json!([{"type": "text", "text": "Hi there"}]));
        stream.events(&json!({"type": "message_end", "message": {"role": "assistant", "content": []}}));
        assert_eq!(
            stream.events(&update(json!({"type": "text_delta", "contentIndex": 0, "delta": "!"}))),
            []
        );
        let whole = json!({"type": "message_update", "message": {"role": "assistant", "content": []}, "frame": {}});
        assert_eq!(stream.events(&whole), [LaneEvent::Streaming(whole["message"].clone())]);
    }

    #[test]
    fn cli_interactive_watch_events_become_tui_events() {
        use client::session::{WORKER_RESTARTED, connection_event};
        assert!(is_worker_restart(&connection_event(
            "reconnecting",
            Some(WORKER_RESTARTED)
        )));
        assert!(!is_worker_restart(&connection_event(
            "reconnecting",
            Some("The Ultron server went away")
        )));
        assert!(!is_worker_restart(&connection_event("rejoined", None)));
        let queued = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "steer", "entryId": "e1", "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}
        ]});
        assert_eq!(
            tui_events(&queued),
            [LaneEvent::Queues(vec![Queued {
                entry_id: "e1".into(),
                kind: QueueKind::Steer,
                text: "go".into(),
                user: true,
            }])]
        );
        let next = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "nextRun", "entryId": "e2", "message": {"role": "user", "content": [{"type": "text", "text": "later"}]}},
            {"type": "message", "kind": "followUp", "entryId": "e3", "message": {"role": "user", "content": [{"type": "text", "text": "after"}]}}
        ]});
        assert_eq!(
            tui_events(&next),
            [LaneEvent::Queues(vec![
                Queued {
                    entry_id: "e2".into(),
                    kind: QueueKind::NextRun,
                    text: "later".into(),
                    user: true,
                },
                Queued {
                    entry_id: "e3".into(),
                    kind: QueueKind::FollowUp,
                    text: "after".into(),
                    user: true,
                }
            ])]
        );
        let host = json!({"type": "queue_update", "lane": "main", "queues": [
            {"type": "message", "kind": "followUp", "entryId": "r1", "message": {"role": "custom", "customType": "wake", "content": "event"}},
            {"type": "custom", "kind": "steer", "entryId": "r2", "customType": "ultron-wake"}
        ]});
        assert_eq!(
            tui_events(&host),
            [LaneEvent::Queues(vec![
                Queued {
                    entry_id: "r1".into(),
                    kind: QueueKind::FollowUp,
                    text: String::new(),
                    user: false,
                },
                Queued {
                    entry_id: "r2".into(),
                    kind: QueueKind::Steer,
                    text: "<ultron-wake>".into(),
                    user: false,
                }
            ])]
        );
        let failed = json!({"type": "run_end", "status": "failed", "error": {"message": "boom"}});
        assert_eq!(
            tui_events(&failed),
            [LaneEvent::OperationEnded {
                error: Some("boom".into())
            }]
        );
        let aborted = json!({"type": "run_end", "status": "aborted"});
        assert_eq!(tui_events(&aborted), [LaneEvent::OperationEnded { error: None }]);
        let user = json!({"type": "message_start", "message": {"role": "user"}});
        assert!(tui_events(&user).is_empty());
    }
}
