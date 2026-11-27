//! The client TUI (`ExperimentalClientTui` in experimental/client-tui.ts): the startup header and transcript above
//! a dock of pending messages, status, the editor (or a selector in its place) and the footer, laid out full
//! screen. Keys follow Pi's: Enter sends (steers while a turn runs), Alt+Enter queues a follow-up, Escape aborts the
//! turn, Ctrl+C clears and exits on a second press, Ctrl+D exits on an empty editor.
//!
//! The app only holds view state. Everything that touches the session is sent as a [`LaneCommand`] and comes back as
//! [`LaneEvent`]s, so handling a key or painting a frame never waits on the network or the disk.

use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::status_git::GitSnapshot;
use crate::status_line::{StatusData, StreamState, ThemeStyle, Usage, VimDisplay, VimStatus, render_status_line};
use serde_json::Value;
use theme::{Bg, Fg, Theme};
use tui::Editor;
use tui::KeybindingsManager;
use tui::VimEditor;
use tui::VimMode;
use tui::truncate_to_width;
use tui::{Attachments, PASTE_END, PASTE_START, PasteAction, chip_band, classify_paste, draft_chips};
use tui::{AutocompleteItem, SlashCommand};
use tui::{ClipboardRead, ClipboardReply, ClipboardService};
use tui::{Component, Text};
use tui::{FramePlan, HistoryBatch, HistoryKind};
use tui::{SelectEvent, SelectItem, SelectList};
use tui::{StackEntry, stack_lines};

use crate::agent_hub::{AgentHub, HubEvent};
use crate::chat::{ChatStyle, ChatView};
use crate::commands::{MarkdownNotice, SessionStats, Slash, hotkeys_markdown, key_display_text, session_info};
use crate::components::{BashExecution, FooterModel, FooterView, bold, key_text, render_footer};
use crate::copy_targets::TranscriptEntry;
use crate::header::StartupHeader;
use crate::history::{PromptHistoryStore, TimedPrompt};
use crate::history_search::{HistorySearchEvent, PromptHistorySearch};
use crate::lane::{
    AutoreviewRequest, BashResult, LaneCommand, LaneEvent, ModelInfo, PromptImage, Queued, SessionListing,
};
use crate::model_hub::{ModelHub, ModelHubEvent};
use crate::qol::{GitSource, NotifyFn, QolSettings, code_highlighter};
use crate::rlm_cell::Highlighter;
use crate::selectors::{SelectorAction, UserMessageSelector};
use crate::session_selector::{SessionAction, SessionSelector};
use crate::settings_selector::{SettingsEvent, SettingsInput, SettingsSelector, format_http_idle};
use crate::transcript_picker::{PickerEvent, PickerMode, PickerOptions, TranscriptPicker};
use crate::tree_selector::{FilterMode, TreeAction, TreeSelector};
use protocol::pi_message::text as message_text;
use protocol::watch::QueueKind;

mod events;
mod input;
mod login;
mod mouse;
mod panels;
mod render;
mod rlm;
mod services;

use mouse::{AppMouse, MouseLayout};

pub const RLM_PANE_AUTO_ENV: &str = "ULTRON_RLM_PANE_AUTO";

/// The error a navigation answers when its branch summary was aborted (session-core `BRANCH_SUMMARY_ABORTED`).
const BRANCH_SUMMARY_ABORTED: &str = "Branch summarization cancelled";
/// What `/extension` answers (session-core `compose::EXTENSIONS_UNSUPPORTED`).
const EXTENSIONS_UNSUPPORTED: &str = "Pi extensions are not supported by ultron-rs";
/// The last choice of `/autoreview on`'s account picker.
const ALL_ACCOUNTS: &str = "All logged-in accounts";

/// Pi exits on a second Ctrl-C within this window.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_millis(500);
/// Pi's double-Esc window (`DOUBLE_ESCAPE_WINDOW_MS`).
const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(500);
/// The RLM pane opens beside the chat from this terminal width; narrower, the full-screen graph opens.
pub const RLM_PANE_MIN_COLUMNS: usize = 120;
/// RLM polls while an RLM view (or the live wave summary) shows, and while nothing does (`RLM_POLL_*_MS`).
const RLM_POLL_VISIBLE: Duration = Duration::from_millis(1000);
const RLM_POLL_HIDDEN: Duration = Duration::from_millis(5000);

/// Suspends the process until it is continued (`suspendTui`); `Err` is shown in the status line.
pub type SuspendHook = Box<dyn FnMut() -> Result<(), String>>;

/// `rlmPaneWidth`: about 40% of the terminal, at least 44 columns (border excluded).
pub(super) fn rlm_pane_width(columns: usize) -> usize {
    44usize.max((columns as f64 * 0.4).round() as usize)
}

/// Whether `ULTRON_SPLASH` / `ULTRON_RLM_PANE_AUTO` turn a default-on feature off (`off`, `0`, `false`).
pub fn env_off(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or("").trim().to_lowercase().as_str(),
        "off" | "0" | "false"
    )
}

/// What the app was started with.
pub struct AppOptions {
    pub theme: Rc<Theme>,
    /// The app's keys; the editor and every dialog match with clones of it.
    pub keys: KeybindingsManager,
    pub version: String,
    pub cwd: String,
    pub home: Option<String>,
    /// Shown as the header's `Server:` line.
    pub server_id: String,
    pub models: Vec<ModelInfo>,
    pub model: ModelInfo,
    pub thinking_level: String,
    pub splash: bool,
    pub history: PromptHistoryStore,
    pub fd_path: Option<std::path::PathBuf>,
    /// The `rlm` cell highlighter (highlight.js Python, as TypeScript); `qol.highlight` covers code blocks only.
    pub highlight: Highlighter,
    /// The Rust-only additions ([`QolSettings::parity`] paints exactly what the TypeScript client TUI paints).
    pub qol: QolSettings,
    /// `/bug`: the host an upload goes to (`new URL(getRadiusGatewayUrl()).host`) and whether the session is offline
    /// (`PI_OFFLINE`).
    pub bug_report_target: (String, bool),
}

/// The file-restoring rewind's choices.
const RESTORE_FILES: &str = "Restore files";
const RESTORE_FILES_AND_REWIND: &str = "Restore files and rewind the conversation";
const RESTORE_CANCEL: &str = "Cancel";

enum Purpose {
    Model,
    Thinking,
    /// `/mode`: the always-on skills the status names after a choice.
    Mode(Vec<String>),
    /// `/autoreview on`: the `gh` accounts, `(login, host)`, to review as.
    AutoreviewAccount(Vec<(String, String)>),
    /// "Summarize branch?" before navigating to this entry.
    Summarize(String),
    /// "Restore files ...?" before restoring the working tree to this entry's checkpoint (the rewind selector's
    /// choices may also rewind the conversation to it).
    RestoreFiles(String),
    /// "Select authentication method": the options it chooses among (`None`: every provider).
    LoginMethod(Vec<crate::selectors::AuthProvider>, bool),
    /// A `/mcp` `select` dialog: the request id.
    ExtensionSelect {
        id: String,
    },
    /// "Switch to it?" after `/import`: the imported session's file, what was imported, and the Pi file it came from.
    SwitchImported {
        session_file: Option<String>,
        imported: String,
        source: String,
    },
}

/// `THINKING_DESCRIPTIONS` of slash-commands-provider.ts.
pub(super) fn thinking_description(level: &str) -> Option<&'static str> {
    Some(match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning",
        "low" => "Light reasoning",
        "medium" => "Moderate reasoning",
        "high" => "Deep reasoning",
        "xhigh" => "Extra-high reasoning",
        "max" => "Maximum reasoning",
        _ => return None,
    })
}

/// A full-screen overlay (the omp-inspired additions, [`QolSettings::overlays`]).
enum Overlay {
    Search(Box<crate::transcript_search::TranscriptSearch>),
    Picker(Box<TranscriptPicker>),
    ModelHub(Box<ModelHub>),
    AgentHub(Box<AgentHub>),
}

/// A component shown in the editor's place (`#showComponent`).
enum Panel {
    Fork(UserMessageSelector),
    HistorySearch(Box<PromptHistorySearch>),
    Tree(Box<TreeSelector>),
    Sessions(Box<SessionSelector>),
    /// Pi's picker of its own sessions (`/import` without a path); a selection is imported, never changed.
    PiSessions(Box<SessionSelector>),
    Settings(Box<SettingsSelector>),
    /// The full-screen RLM graph (`RlmGraphFocus`) in the editor's place.
    RlmFocus(Box<rlm_views::focus::RlmGraphFocus>),
    Logout(Box<crate::selectors::AuthSelector>),
    /// `/trust`: the project trust selector.
    Trust(Box<crate::trust_selector::TrustSelector>),
    /// The `/login` provider selector; `Some(auth type)` when a method was chosen first (Esc goes back to it).
    Login(
        Box<crate::selectors::AuthSelector>,
        Vec<crate::selectors::AuthProvider>,
        Option<String>,
    ),
    /// `/scoped-models`: the Ctrl+P scope selector. `changed`: the user changed the selection since it opened;
    /// `session_scope`: it opened on a session scope (not `enabledModels`).
    ScopedModels {
        selector: Box<crate::scoped_models::ScopedModelsSelector>,
        changed: bool,
        session_scope: bool,
    },
    /// The login dialog of a subscription (OAuth) login running in the client, or of a multi-field API-key login
    /// running in the dialog: the flow its answers and its cancel go to.
    OAuthLogin(Box<crate::login_dialog::LoginDialog>, LoginFlow),
    /// The login dialog's prompt for an API key.
    LoginPrompt {
        provider: crate::selectors::AuthProvider,
        method: String,
        input: Box<tui::Input>,
    },
    /// A `/mcp` `input` dialog (`#showTextInput`): the request id.
    ExtensionInput {
        id: String,
        dialog: Box<crate::extension_dialogs::ExtensionInputDialog>,
    },
    /// The multi-line editor dialog (`#showTextEditor`) for the custom branch summary instructions of navigating to
    /// this entry.
    TextEditor {
        entry: String,
        dialog: Box<crate::extension_dialogs::ExtensionEditorDialog>,
    },
    /// `/share`'s "Creating gist..." loader; Esc cancels the upload.
    ShareLoader(Box<crate::extension_dialogs::BorderedLoader>),
    /// `/bug`'s dialogs and loaders.
    BugReport(Box<crate::bug_report::BugFlow>),
}

/// Who runs the login a [`Panel::OAuthLogin`] dialog shows. The flow lives in the panel, so a dialog that is
/// replaced takes its flow with it: no later login's answers can reach it.
enum LoginFlow {
    /// A subscription login running in the client, by its id ([`LaneCommand::OAuthLogin`]).
    Client(u64),
    /// A multi-field API-key login (`apiKey.login`): its flow and provider.
    ApiKey(Box<(crate::api_key_login::ApiKeyLogin, crate::selectors::AuthProvider)>),
}

/// `nextModel` (client-tui-actions.ts): the next model in the `enabledModels` scope when it selects any
/// (`resolveModelScopeFromModels`: globs, fuzzy ids and names, `:<thinking>` suffixes accepted and not applied,
/// Claude Code CLI models only by their full reference), kept in the available order; else among all available
/// models.
pub fn next_model<'a>(
    available: &'a [ModelInfo],
    current: &ModelInfo,
    direction: isize,
    enabled: &[String],
) -> Result<&'a ModelInfo, &'static str> {
    let mut candidates: Vec<&ModelInfo> = available.iter().collect();
    let mut scoped = false;
    if !enabled.is_empty() {
        let (in_scope, _) = model_scope::resolve_model_scope(enabled, available);
        if !in_scope.is_empty() {
            scoped = true;
            candidates.retain(|m| in_scope.iter().any(|s| model_scope::same(s.model, *m)));
        }
    }
    if candidates.len() <= 1 {
        return Err(if scoped {
            "Only one model in scope"
        } else {
            "Only one model available"
        });
    }
    let index = candidates
        .iter()
        .position(|m| m.provider == current.provider && m.id == current.id)
        .map_or(-1, |i| i as isize);
    let n = candidates.len() as isize;
    Ok(candidates[((index + direction + n) % n) as usize])
}

struct Selection {
    title: String,
    list: SelectList,
    purpose: Purpose,
}

pub struct App {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    editor: Editor,
    chat: ChatView,
    header: StartupHeader,
    lane: std::sync::mpsc::Sender<LaneCommand>,
    /// `/bug`'s upload host and whether the session is offline ([`AppOptions::bug_report_target`]).
    bug_report_target: (String, bool),
    history: PromptHistoryStore,
    cwd: String,
    home: Option<String>,
    server_id: String,
    models: Vec<ModelInfo>,
    model: ModelInfo,
    thinking_level: String,
    status: String,
    operation: Option<String>,
    /// A compaction is running (its indicator says Esc cancels it).
    compacting: bool,
    /// The run is waiting to retry its request (`auto_retry_start`): Esc stops the retry.
    retrying: bool,
    selection: Option<Selection>,
    /// What `/mode` or `/engineering` does once [`LaneEvent::Mode`] reports the settings' mode.
    mode_request: Option<crate::modes::ModeRequest>,
    session_id: Option<String>,
    /// Startup notices that arrived before the first session opened, shown with it.
    early_startup_notices: Vec<crate::lane::StartupNotice>,
    /// `anthropicSubscriptionWarningShown`.
    subscription_warning_shown: bool,
    session_file: Option<String>,
    session_name: Option<String>,
    /// `/name <name>` sent and not yet confirmed: client-tui-commands.ts awaits `control.setName` before it shows the
    /// name and "Session name set", so the notice waits for the worker's `session_name` event with that name.
    pending_name: Option<String>,
    transcript: Vec<Value>,
    last_ctrl_c: Option<Instant>,
    pub exit: bool,
    /// Escape sequences for the terminal (clipboard), written by the run loop.
    pub terminal_writes: Vec<String>,
    /// Terminal rows (the editor shows at most 30% of them; the splash needs room).
    pub rows: usize,
    qol: QolSettings,
    vim: Option<VimEditor>,
    attachments: Attachments,
    clipboard: Option<ClipboardService>,
    git: Option<Box<dyn GitSource>>,
    git_snapshot: GitSnapshot,
    /// The footer's git branch (`FooterDataProvider.getGitBranch`), read off the paint path.
    footer_branch: Option<crate::footer_data::FooterBranch>,
    footer_git_branch: Option<String>,
    notify: Option<NotifyFn>,
    /// Terminal bytes from service threads (notifications), moved into `terminal_writes` by `poll_services`.
    side_writes: Arc<Mutex<Vec<String>>>,
    /// Transcript rows already in the terminal's scrollback (the scrollback/live split).
    committed: Vec<String>,
    history_id: u64,
    /// The user aborted the running turn (no notification for it).
    aborting: bool,
    turn_started: Option<Instant>,
    active_ms: u64,
    ticks: u64,
    /// `/arminsayshi` portraits still animating: the portrait, when it started, and the frames it played.
    armin: Vec<(u64, Rc<std::cell::RefCell<crate::easter_eggs::Armin>>, Instant, u64)>,
    /// The running `!` command (Pi's pending `BashExecutionComponent`).
    bash: Option<BashExecution>,
    /// Transcript messages before this index are compacted away (the context starts after them).
    context_from: usize,
    compacted: bool,
    panel: Option<Panel>,
    /// `enabledModels` (Ctrl+P's scope).
    enabled_models: Vec<String>,
    /// The session-only Ctrl+P scope from `/scoped-models` (`provider/id` references); `None` follows
    /// `enabled_models`, an empty list is every model.
    scoped_ids: Option<Vec<String>>,
    overlay: Option<Overlay>,
    /// Esc-Esc on an idle, empty editor (the rewind selector).
    escapes: crate::keybindings::DoubleTap,
    /// Rewind waits for the session's user-message entries.
    rewind_pending: bool,
    /// The rewind selector's turns: their ids and whether each is a session user entry.
    rewind_ids: Vec<(String, bool)>,
    /// The conversation rewinds to this entry once its file restore succeeded.
    rewind_after_restore: Option<String>,
    /// Models picked this session, newest first (the model hub's Recent scope).
    recent_models: Vec<String>,
    /// The terminal display should be reset (Alt+L): the run loop clears it and paints everything again.
    pub reset_display: bool,
    /// The last tree `/tree` showed (to reopen it after a cancelled summary choice).
    tree: Option<(Value, Option<String>)>,
    tree_filter: FilterMode,
    /// `branchSummary.skipPrompt`: navigate without asking about a summary.
    skip_summary_prompt: bool,
    /// The entry a navigation with a branch summary goes to, while the summary runs: Esc cancels it.
    summarizing_branch: Option<String>,
    /// Usage of the path's branch summaries (`{input, output, cacheRead, cacheWrite, cost}`), in the footer's totals.
    path_usage: Value,
    /// `compaction.enabled` (the footer's `(auto)`).
    auto_compact: bool,
    /// The external editor command (`externalEditor`, `$VISUAL`, `$EDITOR`).
    external_editor: Option<String>,
    /// Ctrl+G asked for the external editor over this text; the run loop runs it.
    edit_request: Option<String>,
    /// Set while something else owns the terminal: the input pump stops reading.
    input_gate: Arc<std::sync::atomic::AtomicBool>,
    /// The connection to a Radius-hosted server is being recovered (client-tui.ts `#busy` during recovery): input
    /// waits, except Ctrl+C and Ctrl+D on an empty editor.
    recovering: bool,
    /// The width of the last frame (transcript search renders the transcript at it).
    cols: usize,
    /// The RLM panel shows (`app.rlm.toggle`).
    rlm_visible: bool,
    /// The last subagent listing (`rlm.list_subagents`).
    rlm_agents: Value,
    rlm_clock: rlm_views::visualizer::RlmClock,
    /// `/mcp` dialogs not shown yet: they share the selector, so they are answered in order.
    extension_dialogs: std::collections::VecDeque<Value>,
    /// The clock (tests drive double taps, countdowns and polls with their own).
    clock: Box<dyn Fn() -> Instant>,
    /// The theme setting, its live application and previews.
    theme_controller: Option<crate::theme_setup::ThemeController>,
    /// The terminal appearance last told to the client ([`LaneCommand::TerminalTheme`]).
    told_terminal_theme: Option<&'static str>,
    /// Takes the OSC 11 background replies out of the input.
    osc11: tui::Osc11Reader,
    /// The terminal's color palette notifications are on (`CSI ? 2031 h` sent, not yet turned off).
    scheme_notifications: bool,
    /// `showHardwareCursor`: the painter shows the terminal's cursor at the editor's.
    pub show_hardware_cursor: bool,
    /// `terminal.clearOnShrink`: the split-viewport painter repaints a shrunk viewport whole.
    pub clear_on_shrink: bool,
    /// The terminal's image protocol on the alternate screen (`TuiAltScreen.imageProtocol`) and whether it is
    /// WezTerm; the painter places and clears images with them.
    pub image_protocol: Option<tui::ImageProtocol>,
    pub wezterm: bool,
    /// `doubleEscapeAction`: `tree`, `fork` or `none`.
    double_escape_action: String,
    last_escape: Option<Instant>,
    /// `ULTRON_SPLASH` allows the splash; `quietStartup` hides it.
    splash_allowed: bool,
    /// Ctrl+Z asked to suspend (`app.suspend`): the run loop gives the terminal back and stops the process.
    suspend_request: bool,
    suspend_hook: Option<SuspendHook>,
    /// Success statuses of copies waiting for the clipboard's answer, oldest first; `None` is a fullscreen
    /// selection copy, which answers with a flash.
    pending_copies: std::collections::VecDeque<Option<String>>,
    /// Clipboard reads waiting for their answer, oldest first: true for a right-click paste, false for Ctrl+V.
    pending_reads: std::collections::VecDeque<bool>,
    /// The fullscreen transcript viewport (`TuiAltScreen`): scroll position, selection, scrollbar, flashes.
    viewport: tui::FullscreenViewport,
    /// The mouse tracking the alternate screen turns on (`None`: none, as in tests that paint frames).
    mouse_tracking: Option<&'static str>,
    /// The TUI debug outputs the environment turned on (`PI_TUI_WRITE_LOG`, `PI_TUI_DEBUG_REDRAW`, `PI_TUI_DEBUG`).
    render_debug: Option<std::sync::Arc<tui::RenderDebug>>,
    /// Running in Termux (`TERMUX_VERSION`): the split viewport diffs a height change.
    termux: bool,
    /// Where a pasted clipboard image is saved (`tmpdir()`).
    temp_dir: std::path::PathBuf,
    /// The RLM pane (`app.rlm.pane`): open beside the chat on wide terminals; focused takes the keys.
    rlm_pane: rlm_views::pane::RlmPane,
    rlm_pane_open: bool,
    rlm_pane_focused: bool,
    /// Runs the pane already opened for (or showed while open), and `op:<id>` for turns in which the user closed it.
    rlm_pane_handled: std::collections::HashSet<String>,
    /// `rlmPaneAutoOpen` (and not `ULTRON_RLM_PANE_AUTO=off`).
    rlm_pane_auto: bool,
    /// The RLM inspections polled besides the subagent listing (`#refreshRlm`).
    rlm_poll: rlm_views::poller::RlmPoller,
    /// When the RLM inspections were last polled (`None`: poll on the next pass).
    rlm_polled: Option<Instant>,
    /// A poll round was sent and its last answer (`detached`) has not come back: the link thread answers commands in
    /// order, so no new round is queued behind it (a busy link thread would otherwise collect a burst of stale ones).
    rlm_round: bool,
    /// The wave summary showed at the last poll (it polls at the visible rate while it does).
    wave_active: bool,
    /// The root lane's detached cells (`rlm.detached`), for the running-work panel.
    detached_cells: Vec<rlm_views::running::DetachedCell>,
    /// The running-work panel had something to show at the last poll (it polls at the visible rate while it does).
    work_active: bool,
    /// The running operation's start, ms since the epoch (the pane's auto-open counts runs from it).
    operation_started_ms: Option<f64>,
    /// The external editor edits the editor dialog's text, not the main editor's.
    edit_dialog: bool,
    /// The id of the last subscription login started ([`LaneCommand::OAuthLogin`]).
    last_login: u64,
    /// Where the mouse-aware parts of the last fullscreen frame were.
    mouse_layout: MouseLayout,
}

pub(super) fn thinking_border(t: &Rc<Theme>, level: &str) -> Box<dyn Fn(&str) -> String> {
    let t = Rc::clone(t);
    let level = level.to_string();
    Box::new(move |s| t.thinking_border(&level, s))
}

/// `calculateContextTokens`.
pub(super) fn context_tokens(usage: &Value) -> u64 {
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    match n("totalTokens") {
        0 => n("input") + n("output") + n("cacheRead") + n("cacheWrite"),
        total => total,
    }
}

/// `estimateTokens`: characters / 4, rounded up.
pub(super) fn estimate_tokens(message: &Value) -> u64 {
    let text_chars = |content: &Value| -> usize {
        match content {
            Value::String(s) => s.encode_utf16().count(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .map(|t| t.encode_utf16().count())
                .sum(),
            _ => 0,
        }
    };
    let chars = match message["role"].as_str() {
        Some("assistant") => message["content"].as_array().map_or(0, |blocks| {
            blocks
                .iter()
                .map(|b| match b["type"].as_str() {
                    Some("text") => b["text"].as_str().unwrap_or("").encode_utf16().count(),
                    Some("thinking") => b["thinking"].as_str().unwrap_or("").encode_utf16().count(),
                    Some("toolCall") => {
                        b["name"].as_str().unwrap_or("").encode_utf16().count()
                            + b["arguments"].to_string().encode_utf16().count()
                    }
                    _ => 0,
                })
                .sum()
        }),
        Some(_) => text_chars(&message["content"]),
        None => 0,
    };
    (chars as u64).div_ceil(4)
}

/// `laneContextUsage`: the last valid assistant usage plus estimates for the messages after it; `None` without a
/// context window.
pub fn context_usage(transcript: &[Value], window: u64) -> Option<(u64, f64)> {
    if window == 0 {
        return None;
    }
    let last = transcript.iter().rposition(|m| {
        m["role"] == "assistant"
            && m["stopReason"] != "aborted"
            && m["stopReason"] != "error"
            && context_tokens(&m["usage"]) > 0
    });
    let tokens = match last {
        Some(i) => {
            context_tokens(&transcript[i]["usage"]) + transcript[i + 1..].iter().map(estimate_tokens).sum::<u64>()
        }
        None => transcript.iter().map(estimate_tokens).sum(),
    };
    Some((tokens, tokens as f64 / window as f64 * 100.0))
}

/// The editor's slash commands: the client TUI's, and `/model`'s completions.
pub(super) fn slash_commands(models: &[ModelInfo], qol: &QolSettings) -> Vec<SlashCommand> {
    let mut commands: Vec<SlashCommand> = Slash::ALL
        .into_iter()
        .filter(|c| c.listed(qol.overlays))
        .map(|c| {
            let c = c.spec();
            SlashCommand {
                name: c.name.to_string(),
                description: Some(c.description.to_string()),
                argument_hint: (!c.hint.is_empty()).then(|| c.hint.to_string()),
                argument_completions: None,
            }
        })
        .collect();
    let models = models.to_vec();
    if let Some(model) = commands.iter_mut().find(|c| c.name == "model") {
        model.argument_completions = Some(Box::new(move |prefix: &str| {
            let normalized = prefix.to_lowercase();
            Some(
                models
                    .iter()
                    .filter(|m| {
                        format!("{}/{} {}", m.provider, m.id, m.name)
                            .to_lowercase()
                            .contains(&normalized)
                    })
                    .map(|m| AutocompleteItem {
                        value: format!("{}/{}", m.provider, m.id),
                        label: m.id.clone(),
                        description: Some(m.provider.clone()),
                    })
                    .collect(),
            )
        }));
    }
    let fixed: [(&str, &'static [&'static str]); 4] = [
        ("engineering", &["true", "false"]),
        ("autoreview", &["on", "off", "ban", "unban"]),
        ("persona", &["on", "off", "auto", "ultron", "none"]),
        ("keepgoing", &["on", "off"]),
    ];
    for (name, values) in fixed {
        if let Some(command) = commands.iter_mut().find(|c| c.name == name) {
            command.argument_completions = Some(Box::new(move |prefix: &str| {
                Some(
                    values
                        .iter()
                        .filter(|value| value.starts_with(prefix))
                        .map(|value| AutocompleteItem {
                            value: value.to_string(),
                            label: value.to_string(),
                            description: None,
                        })
                        .collect(),
                )
            }));
        }
    }
    commands
}

impl App {
    pub fn new(options: AppOptions, lane: std::sync::mpsc::Sender<LaneCommand>) -> Self {
        let theme = options.theme;
        let mut editor = Editor::new(
            options.keys.clone(),
            thinking_border(&theme, &options.thinking_level),
            1,
        );
        editor.set_select_list_theme(crate::components::select_theme(&theme, Fg::Muted, Fg::Muted));
        let qol = options.qol;
        let commands = slash_commands(&options.models, &qol);
        editor.set_autocomplete_provider(crate::qol::autocomplete_provider(
            commands,
            &options.cwd,
            options.fd_path.clone(),
            &options.models,
            &qol,
        ));
        let mut history = options.history;
        let entries = history.load();
        editor.set_history(&entries.iter().map(String::as_str).collect::<Vec<_>>());
        let keys = options.keys;
        let style = ChatStyle {
            theme: Rc::clone(&theme),
            highlight: options.highlight,
            code_highlight: qol.highlight.then(|| code_highlighter(&theme)),
            expand_key: key_text(&keys, "app.tools.expand"),
            dequeue_key: key_display_text(&keys, "app.message.dequeue"),
            interrupt_key: key_text(&keys, "app.interrupt"),
        };
        App {
            header: StartupHeader {
                theme: Rc::clone(&theme),
                version: options.version,
                expanded: false,
                details: String::new(),
                splash: options.splash,
                rows: None,
            },
            chat: ChatView::new(style),
            theme,
            keys,
            editor,
            lane,
            bug_report_target: options.bug_report_target,
            history,
            cwd: options.cwd,
            home: options.home,
            server_id: options.server_id,
            models: options.models,
            model: options.model,
            thinking_level: options.thinking_level,
            status: "Starting Session…".into(),
            operation: None,
            compacting: false,
            retrying: false,
            selection: None,
            mode_request: None,
            session_id: None,
            early_startup_notices: Vec::new(),
            subscription_warning_shown: false,
            session_file: None,
            session_name: None,
            pending_name: None,
            transcript: Vec::new(),
            last_ctrl_c: None,
            exit: false,
            terminal_writes: Vec::new(),
            rows: 24,
            vim: qol.vim.then(VimEditor::new),
            qol,
            attachments: Attachments::default(),
            clipboard: None,
            git: None,
            git_snapshot: GitSnapshot::default(),
            footer_branch: None,
            footer_git_branch: None,
            notify: None,
            side_writes: Arc::default(),
            committed: Vec::new(),
            history_id: 0,
            aborting: false,
            turn_started: None,
            active_ms: 0,
            ticks: 0,
            armin: Vec::new(),
            bash: None,
            context_from: 0,
            compacted: false,
            panel: None,
            extension_dialogs: std::collections::VecDeque::new(),
            enabled_models: Vec::new(),
            scoped_ids: None,
            overlay: None,
            escapes: Default::default(),
            rewind_pending: false,
            rewind_ids: Vec::new(),
            rewind_after_restore: None,
            recent_models: Vec::new(),
            reset_display: false,
            tree: None,
            tree_filter: FilterMode::Default,
            skip_summary_prompt: false,
            summarizing_branch: None,
            path_usage: Value::Null,
            auto_compact: true,
            external_editor: None,
            edit_request: None,
            input_gate: Arc::default(),
            recovering: false,
            cols: 80,
            rlm_visible: false,
            rlm_agents: Value::Null,
            rlm_clock: rlm_views::visualizer::RlmClock::new(),
            clock: Box::new(Instant::now),
            theme_controller: None,
            told_terminal_theme: None,
            osc11: tui::Osc11Reader::new(),
            scheme_notifications: false,
            show_hardware_cursor: false,
            image_protocol: None,
            wezterm: false,
            clear_on_shrink: false,
            double_escape_action: "tree".into(),
            last_escape: None,
            splash_allowed: options.splash,
            suspend_request: false,
            suspend_hook: None,
            pending_copies: std::collections::VecDeque::new(),
            pending_reads: std::collections::VecDeque::new(),
            viewport: {
                let mut viewport = tui::FullscreenViewport::default();
                viewport.keep_zone_marks = true;
                viewport
            },
            mouse_tracking: None,
            render_debug: None,
            termux: false,
            temp_dir: jsrt::fs::temp_dir(),
            rlm_pane: rlm_views::pane::RlmPane::new(),
            rlm_pane_open: false,
            rlm_pane_focused: false,
            rlm_pane_handled: std::collections::HashSet::new(),
            rlm_pane_auto: true,
            rlm_poll: rlm_views::poller::RlmPoller::new(rlm_views::poller::TUI_FRAME_LIMIT),
            rlm_polled: None,
            rlm_round: false,
            wave_active: false,
            detached_cells: Vec::new(),
            work_active: false,
            operation_started_ms: None,
            edit_dialog: false,
            last_login: 0,
            mouse_layout: MouseLayout::default(),
        }
        .with_cancel_key()
    }

    /// `setKittyProtocolActive` (the terminal's keyboard protocol negotiation): every key manager, the editor and
    /// the Vim layer match Kitty input (`shift+enter` from `ESC CR` or LF, ...).
    pub fn set_kitty_protocol_active(&mut self, active: bool) {
        self.keys.set_kitty_active(active);
    }

    /// Whether key matching treats the input as Kitty keyboard protocol input.
    pub fn kitty_protocol_active(&self) -> bool {
        self.keys.ctx().kitty_active
    }

    /// `KeybindingsManager.setUserBindings`: the app's keys, the editor's and every component built from them
    /// afterwards resolve `user`.
    pub fn set_user_keybindings(&mut self, user: tui::UserBindings) {
        self.keys.set_user_bindings(user.clone());
        self.editor.set_user_keybindings(user);
        self.chat.cancel_key = key_text(&self.keys, "tui.select.cancel");
    }

    fn with_cancel_key(mut self) -> Self {
        self.chat.cancel_key = key_text(&self.keys, "tui.select.cancel");
        self
    }

    /// Pi's `hideThinkingBlock` setting.
    pub fn set_hide_thinking(&mut self, hide: bool) {
        self.chat.set_hide_thinking(hide);
    }

    /// The external editor command (`getExternalEditorCommand`).
    pub fn set_external_editor(&mut self, command: Option<String>) {
        self.external_editor = command.filter(|c| !c.trim().is_empty());
    }

    /// The flag that pauses the input pump ([`crate::run::spawn_gated_input`]).
    pub fn input_gate(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.input_gate)
    }

    /// The pending Ctrl+G request: `(command, text)`.
    pub fn take_edit_request(&mut self) -> Option<(String, String)> {
        let content = self.edit_request.take()?;
        Some((self.external_editor.clone()?, content))
    }

    /// The editor's result: the new editor text, or `None` (the editor failed: the text stays).
    pub fn finish_edit(&mut self, edited: Option<String>) {
        if std::mem::take(&mut self.edit_dialog) {
            match (edited, self.panel.as_mut()) {
                (Some(text), Some(Panel::TextEditor { dialog, .. })) => dialog.set_text(&text),
                (Some(text), Some(Panel::BugReport(flow))) => {
                    if let Some(dialog) = flow.editor() {
                        dialog.set_text(&text);
                    }
                }
                _ => {}
            }
            return;
        }
        if let Some(text) = edited {
            self.editor.set_text(&text);
            self.update_editor_border();
        }
    }

    /// `compaction.enabled`.
    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.auto_compact = enabled;
    }

    /// Pi's `treeFilterMode` and `branchSummary.skipPrompt` settings.
    pub fn set_tree_settings(&mut self, filter: &str, skip_summary_prompt: bool) {
        self.tree_filter = FilterMode::parse(filter);
        self.skip_summary_prompt = skip_summary_prompt;
    }

    /// Pi's `enabledModels` setting: the models Ctrl+P cycles through.
    pub fn set_enabled_models(&mut self, patterns: Vec<String>) {
        self.enabled_models = patterns;
    }

    /// The clock the app reads for double taps, countdowns and RLM polls (tests pass their own).
    pub fn set_clock(&mut self, clock: Box<dyn Fn() -> Instant>) {
        self.clock = clock;
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    /// Where a pasted clipboard image is saved (default: the system temp directory).
    pub fn set_temp_dir(&mut self, dir: std::path::PathBuf) {
        self.temp_dir = dir;
    }

    /// The presentation settings the TUI applies to itself at startup (`runClientTui`): the hardware cursor and
    /// clear-on-shrink, the double-Esc action, `quietStartup` (no splash), `rlmPaneAutoOpen` (with
    /// `ULTRON_RLM_PANE_AUTO`); and an explicitly set `editorPaddingX` and `autocompleteMaxVisible`.
    pub fn session_name(&self) -> Option<&str> {
        self.session_name.as_deref()
    }

    /// Whether something animates (the working spinner, a loader, a dialog's countdown).
    pub fn animating(&self) -> bool {
        self.chat.is_working()
            || self.bash.is_some()
            || matches!(self.panel, Some(Panel::ShareLoader(_)))
            || matches!(&self.panel, Some(Panel::BugReport(flow)) if flow.animating())
            || !self.armin.is_empty()
            || matches!(&self.panel, Some(Panel::ExtensionInput { dialog, .. }) if dialog.counting())
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
        self.chat.tick();
        if let Some(Panel::ShareLoader(loader)) = self.panel.as_mut() {
            loader.tick();
        }
        if let Some(Panel::BugReport(flow)) = self.panel.as_mut() {
            flow.tick();
        }
        let now = self.now();
        let chat = &mut self.chat;
        self.armin.retain_mut(|(id, armin, started, played)| {
            let mut armin = armin.borrow_mut();
            let due = now.saturating_duration_since(*started).as_millis() as u64 * armin.fps() / 1000;
            let before = *played;
            while *played < due && !armin.done() {
                armin.tick();
                *played += 1;
            }
            if *played != before || armin.done() {
                chat.refresh_live(*id, armin.done());
            }
            !armin.done()
        });
        self.expire_dialogs();
        if let Some(bash) = self.bash.as_mut() {
            bash.tick();
        }
    }

    /// Called off the UI thread when an autocomplete answer computed there (`@` file search) is ready; the run
    /// loop then applies it with [`App::poll_autocomplete`].
    pub fn set_autocomplete_waker(&mut self, waker: impl Fn() + Send + Sync + 'static) {
        self.editor.set_autocomplete_waker(waker);
    }

    /// Apply autocomplete answers computed off the UI thread; true when the editor changed. Never blocks.
    pub fn poll_autocomplete(&mut self) -> bool {
        self.editor.poll_autocomplete()
    }

    /// Whether a debounced autocomplete request waits.
    pub fn fire_autocomplete_pending(&self) -> bool {
        self.editor.has_pending_autocomplete()
    }

    /// Fire the editor's debounced autocomplete request, when one waits.
    pub fn fire_autocomplete(&mut self) -> bool {
        if self.editor.has_pending_autocomplete() {
            self.editor.fire_autocomplete_debounce();
            return true;
        }
        false
    }

    fn update_editor_border(&mut self) {
        let bash = self.editor.text().trim_start().starts_with('!');
        self.editor.border_color = if bash {
            let t = Rc::clone(&self.theme);
            Box::new(move |s| t.fg(Fg::BashMode, s))
        } else {
            thinking_border(&self.theme, &self.thinking_level)
        };
    }

    /// Pi's terminal title: app, session name, cwd.
    fn update_title(&mut self) {
        let cwd = std::path::Path::new(&self.cwd)
            .file_name()
            .map_or_default(|n| n.to_string_lossy().into_owned());
        let title = match &self.session_name {
            Some(name) => format!("ultron - {name} - {cwd}"),
            None => format!("ultron - {cwd}"),
        };
        self.terminal_writes.push(format!("\x1b]0;{title}\x07"));
    }

    /// One of interactive-mode.ts's startup notices (crate::startup_notices).
    fn show_startup_notice(&mut self, notice: crate::lane::StartupNotice) {
        use crate::lane::StartupNotice;
        use crate::startup_notices::{
            ANTHROPIC_SUBSCRIPTION_AUTH_WARNING, NewVersionNotice, WhatsNewNotice, warning_text,
        };
        match notice {
            StartupNotice::WhatsNew {
                markdown,
                collapse,
                version,
            } => self.chat.append_notice(Box::new(WhatsNewNotice {
                theme: Rc::clone(&self.theme),
                markdown,
                collapse,
                version,
                code_block_indent: None,
                hyperlinks: crate::render_settings::hyperlinks(),
            })),
            StartupNotice::NewVersion(version) => self.chat.append_notice(Box::new(NewVersionNotice {
                theme: Rc::clone(&self.theme),
                version,
                hyperlinks: crate::render_settings::hyperlinks(),
            })),
            StartupNotice::SubscriptionAuth => {
                if !self.subscription_warning_shown {
                    self.subscription_warning_shown = true;
                    let text = warning_text(&self.theme, ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
                    self.notice(&text);
                }
            }
        }
    }

    /// `maybeWarnAboutAnthropicSubscriptionAuth`: only an `anthropic` model is checked, and only until it warned.
    fn check_subscription_auth(&mut self) {
        if !self.subscription_warning_shown && self.model.provider == "anthropic" {
            self.send(LaneCommand::SubscriptionAuthWarning(self.model.provider.to_string()));
        }
    }

    pub fn set_image_protocol(&mut self, protocol: Option<tui::ImageProtocol>, wezterm: bool) {
        self.image_protocol = protocol;
        self.wezterm = wezterm;
        let mut images = self.chat.image_settings();
        images.protocol = protocol;
        self.chat.set_image_settings(images);
        if protocol.is_some() {
            self.terminal_writes.push("\x1b[16t".into());
        }
    }
}

/// `buffer` holding `rows`: each existing string is overwritten in place, keeping its allocation.
pub(super) fn refill<'a>(buffer: &mut Vec<String>, rows: impl Iterator<Item = &'a String>) {
    let mut len = 0;
    for row in rows {
        match buffer.get_mut(len) {
            Some(slot) => slot.clone_from(row),
            None => buffer.push(row.clone()),
        }
        len += 1;
    }
    buffer.truncate(len);
}

/// Pi's path argument (`pathArgument`): the first word, or a quoted string.
pub(super) fn path_argument(args: &str) -> Option<String> {
    let text = args.trim_start();
    let quote = text.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &text[1..];
        return rest.find(quote).map(|end| rest[..end].to_string());
    }
    Some(match text.find(char::is_whitespace) {
        Some(space) => text[..space].to_string(),
        None => text.to_string(),
    })
}

/// `/^\x1b\[6;(\d+);(\d+)t$/`: the terminal's cell size answer as (height, width) in pixels.
pub(super) fn parse_cell_size_response(data: &str) -> Option<(u32, u32)> {
    let body = data.strip_prefix("\x1b[6;")?.strip_suffix('t')?;
    let (height, width) = body.split_once(';')?;
    let digits = |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u32>().ok())?;
    Some((digits(height)?, digits(width)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_path_argument_like_ts() {
        assert_eq!(path_argument(""), None);
        assert_eq!(path_argument("   "), None);
        assert_eq!(path_argument("out.html rest"), Some("out.html".into()));
        assert_eq!(path_argument("  a\tb"), Some("a".into()));
        assert_eq!(path_argument("\"my file.jsonl\" x"), Some("my file.jsonl".into()));
        assert_eq!(path_argument("'q'"), Some("q".into()));
        assert_eq!(path_argument("\"open"), None);
    }

    #[test]
    fn app_cell_size_answer_like_consume_cell_size_response() {
        assert_eq!(parse_cell_size_response("\x1b[6;20;10t"), Some((20, 10)));
        assert_eq!(parse_cell_size_response("\x1b[6;0;10t"), Some((0, 10)));
        assert_eq!(parse_cell_size_response("\x1b[6;20t"), None);
        assert_eq!(parse_cell_size_response("\x1b[6;a;10t"), None);
        assert_eq!(parse_cell_size_response("x\x1b[6;20;10t"), None);
    }
}
