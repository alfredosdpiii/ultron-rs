//! What the interactive TUI exchanges with the session it shows, as plain data: events from the lane (Pi message
//! JSON, queue and operation changes) and the commands it sends ([`LaneCommand`]). The TUI never links the engine;
//! the composition root receives its commands and sends its events.

use serde_json::Value;

/// A model the session can switch to (`ModelSummary`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub context_window: u64,
    /// The thinking levels the model supports (`getThinkingLevels`), lowest first; empty: Pi's default set.
    pub thinking_levels: Vec<String>,
}

impl model_scope::ScopeModel for ModelInfo {
    fn provider(&self) -> &str {
        &self.provider
    }
    fn id(&self) -> &str {
        &self.id
    }
    fn name(&self) -> &str {
        &self.name
    }
    /// The Claude Code CLI provider's models (`claude-code`, all on its `claude-code-cli` API) and the Claude
    /// subscription's, whose ids are Anthropic's own.
    fn full_reference_only(&self) -> bool {
        self.provider == "claude-code" || self.provider == "claude-subscription"
    }
}

impl ModelInfo {
    /// The model's thinking levels: its own, else `off` for a model without reasoning, else Pi's five.
    pub fn levels(&self) -> Vec<String> {
        if !self.thinking_levels.is_empty() {
            return self.thinking_levels.clone();
        }
        let levels: &[&str] = if self.reasoning {
            &model_scope::THINKING_LEVELS[..5]
        } else {
            &["off"]
        };
        levels.iter().map(|l| (*l).to_string()).collect()
    }
}

/// `/bug`'s choices (bug-report.ts `BugReportOptions`, with the summary written).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BugReportRequest {
    pub hint: Option<String>,
    pub include_session: bool,
    /// The model's summary, when the user chose one instead of the transcript.
    pub summary: Option<String>,
    /// Upload it (else export a zip).
    pub upload: bool,
}

/// How a `/bug` report's delivery ended.
#[derive(Clone, Debug, PartialEq)]
pub enum BugReportOutcome {
    /// Uploaded: the report id.
    Uploaded(String),
    /// Exported: the archive and the report id.
    Exported { path: String, id: String },
    /// The upload failed (the fallback is offered).
    UploadFailed(String),
    /// Building or writing the report failed (`showError`).
    Failed(String),
}

/// What a `!` command left (`BashResult`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BashResult {
    pub output: String,
    pub exit_code: Option<i64>,
    pub cancelled: bool,
    pub truncated: bool,
    pub full_output_path: Option<String>,
}

/// A user message a fork can start before (`/fork`).
#[derive(Clone, Debug, PartialEq)]
pub struct ForkMessage {
    pub entry_id: String,
    pub text: String,
}

/// A queued item (`LaneSnapshot.queues` item): a message, or a host item shown as `<customType>`.
#[derive(Clone, Debug, PartialEq)]
pub struct Queued {
    pub entry_id: String,
    /// `steer`, `followUp` or `nextRun`.
    pub kind: QueueKind,
    pub text: String,
    /// A `type: "message"` item with `role: "user"`: what the user typed. Only these return to the editor
    /// (Esc, Alt+Up) and show the dequeue hint; queued runtime events (custom messages) stay queued.
    pub user: bool,
}

pub use protocol::watch::QueueKind;

/// A stored session the resume picker offers.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionListing {
    pub path: String,
    pub id: String,
    pub name: Option<String>,
    /// Latest activity, ms since the epoch.
    pub modified: i64,
    pub message_count: usize,
    pub first_message: String,
    /// Every user and assistant message's text, joined by spaces (`SessionInfo.allMessagesText`); the selector
    /// searches it.
    pub all_messages_text: String,
    /// The session's working directory.
    pub cwd: String,
}

/// The session the TUI shows after it opened.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionOpened {
    pub session_id: String,
    pub session_file: Option<String>,
    pub name: Option<String>,
    /// Transcript messages (Pi JSON), oldest first.
    pub messages: Vec<Value>,
}

/// A notice interactive-mode.ts shows around startup.
#[derive(Clone, Debug, PartialEq)]
pub enum StartupNotice {
    /// `showStartupNoticesIfNeeded`: the new changelog entries; `collapse` is the `collapseChangelog` setting.
    WhatsNew {
        markdown: String,
        collapse: bool,
        version: String,
    },
    /// `showNewVersionNotification`: the newer release's version.
    NewVersion(String),
    /// `showWarning` with the Anthropic subscription-auth warning (once per run).
    SubscriptionAuth,
}

/// How a `/scoped-models` catalog refresh ended.
#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRefresh {
    /// Finished; the providers that could not be refreshed (`result.errors`).
    Done(Vec<String>),
    /// The 15 s budget ran out (`result.aborted && timedOut`).
    TimedOut,
    /// The refresh itself failed (the `.catch` branch).
    Failed(String),
}

/// What a login flow reported (`AuthEvent`), as the dialog shows it.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginEvent {
    AuthUrl {
        url: String,
        instructions: Option<String>,
    },
    DeviceCode {
        user_code: String,
        verification_uri: String,
    },
    /// A message and its links (`label`, `url`).
    Info {
        message: String,
        links: Vec<(Option<String>, String)>,
    },
    Progress(String),
}

/// What a login flow asks (`AuthPrompt`).
#[derive(Clone, Debug, PartialEq)]
pub enum LoginPrompt {
    /// Choose one option (`id`, `label`).
    Select {
        message: String,
        options: Vec<(String, String)>,
    },
    /// Paste the authorization code or redirect URL.
    ManualCode { message: String },
    /// A text or secret answer.
    Text {
        message: String,
        placeholder: Option<String>,
        secret: bool,
    },
}

/// One step of a subscription login.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginStep {
    /// The dialog opens (`reach`: `browserReach()`, `local`, `container` or `ssh`).
    Started {
        name: String,
        reach: String,
    },
    Event(LoginEvent),
    Prompt(LoginPrompt),
    /// The flow ended (success, failure or cancel); its outcome arrives as statuses.
    Finished,
}

/// Lane to TUI.
#[derive(Clone, Debug, PartialEq)]
pub enum LaneEvent {
    /// `<agentDir>/keybindings.json` read again (`/reload`): the user bindings every key manager resolves.
    Keybindings(tui::UserBindings),
    /// A session was opened (startup, `/new`, `/clear`, resume).
    Opened(SessionOpened),
    /// The opened session's lane entries on its branch (format-4 entries: messages, compactions with their retained
    /// tails, branch summaries); the transcript is drawn from them as the client TUI draws its lane snapshot.
    Entries(Vec<Value>),
    /// A turn started (operation id).
    OperationStarted(String),
    /// The streaming assistant message so far.
    Streaming(Value),
    /// A message ended and joined the transcript.
    Message(Value),
    /// A tool call started executing.
    ToolStarted { id: String, name: String, args: Value },
    /// A tool call ended (its result message follows as [`LaneEvent::Message`]).
    ToolEnded { id: String },
    /// The queued user messages now.
    Queues(Vec<Queued>),
    /// The turn ended; `error` when it failed to run at all.
    OperationEnded { error: Option<String> },
    /// The session's display name changed (`None` cleared).
    Name(Option<String>),
    /// The selected model changed.
    Model(ModelInfo),
    /// The sessions of this working directory, newest first.
    Sessions(Vec<SessionListing>),
    /// Pi's own sessions of this working directory (`/import` without a path), newest first.
    PiSessions(Vec<SessionListing>),
    /// A line for the status area (`showStatus`).
    Status(String),
    /// The recovery of a Radius-hosted server's connection (client-tui.ts `#handleConnectionState` /
    /// `#handleAttachmentState`): while `busy`, input waits; `status` replaces the status line (empty clears it).
    Recovery { busy: bool, status: String },
    /// A failure outside a turn.
    Error(String),
    /// Command output for the transcript (`host.notice(new Text(text, 1, 0))`).
    Notice(String),
    /// The session goal's reply to `/goal` (shown dim, as client-tui-commands.ts does).
    GoalReply(String),
    /// A `!` command finished (its message follows as [`LaneEvent::Message`] with role `bashExecution`), or failed
    /// to run.
    BashDone(Result<BashResult, String>),
    /// `/bug`'s summary: the text, or why there is none.
    BugSummary(Result<String, String>),
    /// `/bug`'s report was delivered, or not.
    BugReport(BugReportOutcome),
    /// A compaction started (`compaction_start`): `reason` is `manual`, or `threshold` / `overflow` for one the
    /// session runs on its own during a run.
    CompactionStarted { reason: String },
    /// An automatic compaction ended without a compaction entry (`compaction_end`): `declined` (a hook trimmed the
    /// context instead), `failed` with its error, or `aborted`.
    CompactionEnded { status: String, error: Option<String> },
    /// A compaction ended: `Ok` with the compaction entry (`entry_added` of a `compaction` entry), else why it was
    /// rejected.
    Compacted(Result<Value, String>),
    /// A compaction's or branch summary's request failed and is retried (`summarization_retry_scheduled`): the
    /// retry's number, the retries allowed, the delay and the provider's error.
    SummarizationRetryScheduled {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
        error: String,
    },
    /// The retry starts (`summarization_retry_attempt_start`): of a branch summary, or of a compaction for `reason`.
    SummarizationRetryStarted { branch_summary: bool, reason: String },
    /// The summary's retries ended (`summarization_retry_finished`).
    SummarizationRetryFinished,
    /// The run's request failed and is retried after `delay_ms` (`auto_retry_start`).
    AutoRetryStart {
        attempt: u64,
        max_attempts: u64,
        delay_ms: u64,
    },
    /// The run's retries ended (`auto_retry_end`): `Some(error)` when the last attempt failed too.
    AutoRetryEnd { attempt: u64, failed: Option<String> },
    /// The session's thinking level changed.
    Thinking(String),
    /// The user messages `/fork` offers (every branch), oldest first.
    ForkMessages(Vec<ForkMessage>),
    /// The session tree (Pi's `getTree()` nodes, labels resolved) and its current leaf, for `/tree`.
    Tree { tree: Value, leaf: Option<String> },
    /// A tree navigation ended (after the session reopened at its new position), with the selected user or custom
    /// message's text for the editor (AgentSession `navigateTree`'s `editorText`).
    Navigated(Result<Option<String>, String>),
    /// A file-restoring rewind ended: the session's message (`Err`: why nothing, or not everything, was restored).
    FilesRestored(Result<String, String>),
    /// A session was deleted (`Err`: why not); a fresh [`LaneEvent::Sessions`] follows.
    SessionDeleted(Result<(), String>),
    /// The settings `/settings` shows: the merged settings document, the model settings environment variables
    /// override (key -> (variable, value)), the available themes and the terminal's appearance (`light`/`dark`).
    Settings {
        settings: Value,
        env_overrides: std::collections::BTreeMap<String, (String, String)>,
        themes: Vec<String>,
        terminal_theme: String,
    },
    /// The settings' mode for `/mode` and `/engineering`: engineering mode and the always-on skills it names.
    Mode {
        engineering: bool,
        always_skills: Vec<String>,
    },
    /// The settings' `persona` for `/persona` (`None`: unset).
    Persona(Option<String>),
    /// The accounts `gh` is logged in to, as `(login, host)`, for `/autoreview on`.
    AutoreviewAccounts(Vec<(String, String)>),
    /// The stored credentials (`auth.json`): provider id and type (`api_key` or `oauth`), for `/logout`.
    Credentials(Vec<(String, String)>),
    /// What `/login` offers: every sign-in option with what is configured, for the query `/login` was given.
    LoginProviders {
        providers: Vec<crate::selectors::AuthProvider>,
        query: String,
    },
    /// A subscription login's progress (the flow runs in the client process).
    Login(LoginStep),
    /// What `/changelog` shows, lexed off the UI thread ([`crate::changelog::changelog_tokens`]).
    Changelog(Vec<tui::Token>),
    /// The models the session can use now (after a login or logout).
    Catalog(Vec<ModelInfo>),
    /// The model catalogs refreshed ([`LaneCommand::RefreshCatalog`]), or why not.
    CatalogRefreshed(CatalogRefresh),
    /// A startup notice from the client ([`crate::startup_notices`]); shown once the session is open.
    StartupNotice(StartupNotice),
    /// The session's subagents for the RLM views (`rlm.list_subagents`: `{subagents: [...]}`).
    RlmStatus(Value),
    /// The session's subagents (`rlm.list_subagents`: `{subagents: [...]}`), for the agent hub.
    Agents(Value),
    /// A subagent's transcript (`agents.transcript`): its messages, for the agent hub's transcript view.
    AgentTranscript {
        id: String,
        messages: Result<Vec<Value>, String>,
    },
    /// What the agent hub's steer (`agent_message.send`'s `{state, steered}`) or revive (`{id}`) of `id` answered.
    AgentAnswered {
        id: String,
        action: AgentAction,
        answer: Result<Option<Value>, String>,
    },
    /// A read-only inspection's answer ([`LaneCommand::Inspect`]), rendered under `title`.
    Inspected {
        title: String,
        result: Result<Value, String>,
    },
    /// The TUI moved to a fork or clone of the session (after [`LaneEvent::Opened`]): the editor text it starts with
    /// and the status to show.
    Forked { editor_text: String, status: String },
    /// `/import` imported a Pi session (`imported`: what to say about it, e.g. "Imported 16 entries from ... as session
    /// <id>"): `session_file` is the native session to switch to, `current` when that is the attached session.
    PiImported {
        imported: String,
        source: String,
        session_file: Option<String>,
        current: bool,
    },
    /// `/trust`: the session's trust choices, saved decision and current trust.
    Trust(crate::trust_selector::TrustView),
    /// `/debug` wrote its log to `path`; the notice shows it with `summary`.
    DebugLogWritten { path: String, summary: Vec<String> },
    /// `/share`'s gist upload started (`true`: the "Creating gist..." loader shows) or ended (`false`).
    ShareLoader(bool),
    /// An RLM inspection's answer ([`LaneCommand::RlmInspect`]).
    RlmInspected { tag: String, result: Result<Value, String> },
    /// Every project's sessions, newest first (the resume selector's All scope).
    AllSessions(Vec<SessionListing>),
    /// Pi's sessions of every project, newest first (`/import`'s All scope).
    PiAllSessions(Vec<SessionListing>),
    /// A dialog or notification of `/mcp` or the MCP gateway, in Pi's RPC wire format (`extension_ui_request`:
    /// `select`, `input`, `notify`).
    ExtensionUi(Value),
}

/// What the agent hub asked of a subagent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAction {
    Steer,
    Revive,
}

/// An image attached to a prompt (`ImageContent`: base64 data and MIME type).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptImage {
    pub data: String,
    pub mime_type: String,
}

/// TUI to lane: what the TUI asks of the session, sent on the App's channel. Sending never waits; answers arrive as
/// [`LaneEvent`]s.
#[derive(Clone, Debug, PartialEq)]
pub enum LaneCommand {
    /// Answer a `/mcp` dialog (`ultron.extension-ui.respond`): `{value}` or `{cancelled: true}`.
    ExtensionUiRespond {
        id: String,
        answer: Value,
    },
    /// Start a turn with the draft's `[Image #N]` attachments. The first prompt of an unnamed session also names it
    /// (shown at once, refined in the background), unless the user names it first.
    Prompt {
        text: String,
        images: Vec<PromptImage>,
    },
    /// Queue a steering message into the running turn, or a follow-up for when it would stop, under `entry_id`.
    Enqueue {
        kind: QueueKind,
        text: String,
        images: Vec<PromptImage>,
        entry_id: String,
    },
    /// Drop a queued message.
    CancelQueued(String),
    /// Abort the running turn.
    Abort,
    /// Stop the run's retry wait (`abortRetry`); the run then ends with its error.
    AbortRetry,
    NewSession,
    /// `/import <path>`: import the Pi session file at `path` through the server ([`LaneEvent::PiImported`] or a
    /// [`LaneEvent::Status`] error answers).
    ImportPi(String),
    /// `/import` without a path: Pi's sessions of this working directory for Pi's picker ([`LaneEvent::PiSessions`]).
    /// Pi's files are only read.
    ListPiSessions,
    /// Switch to the stored session at this path.
    Resume(String),
    /// The sessions of this working directory ([`LaneEvent::Sessions`]).
    ListSessions,
    /// Name the session (a user-given name: an automatic name never replaces it).
    SetName(String),
    SelectModel {
        provider: String,
        id: String,
    },
    /// Run a `!` command in the session's shell (`exclude`: `!!`, kept out of the model's context);
    /// [`LaneEvent::BashDone`] answers.
    Bash {
        command: String,
        exclude: bool,
    },
    /// Cancel the running `!` command.
    AbortBash,
    /// Compact the session context now ([`LaneEvent::Compacted`] answers).
    Compact(Option<String>),
    /// Set the thinking level ([`LaneEvent::Thinking`] answers).
    SetThinking(String),
    /// The user messages a fork can start before ([`LaneEvent::ForkMessages`]).
    ForkMessages,
    /// Fork the session before user message entry `0` and switch to the fork.
    Fork(String),
    /// Copy the session at its current position and switch to the copy.
    CloneSession,
    /// Export the session (`/export [path]`: HTML, or the Pi JSONL for a `.jsonl` path); the page uses `theme`, the
    /// name of the TUI's active theme (`generateHtml(..., theme.name)`).
    Export {
        path: Option<String>,
        theme: Option<String>,
    },
    /// The session report (`/usage`), as a [`LaneEvent::Notice`].
    Usage,
    /// `/goal [objective | pause | resume | clear | check <command>]` ([`LaneEvent::GoalReply`] answers).
    Goal(String),
    /// Read the session tree ([`LaneEvent::Tree`]).
    Tree,
    /// Navigate to the selected tree entry `target` (AgentSession `navigateTree`: before a user or custom message,
    /// else onto the entry; `None`: the root), optionally summarizing the branch it leaves
    /// ([`LaneEvent::Navigated`]).
    Navigate {
        target: Option<String>,
        summarize: bool,
        instructions: Option<String>,
    },
    /// Label a tree entry (`None` removes it).
    SetLabel {
        entry_id: String,
        label: Option<String>,
    },
    /// Restore the working tree's files to the checkpoint of this entry (its turn's start), the current files saved
    /// as a checkpoint first ([`LaneEvent::FilesRestored`]).
    RestoreFiles(String),
    /// The session's subagents ([`LaneEvent::Agents`]).
    Agents,
    /// A read-only inspection request (`/memory list`, `/agents <id>`, ...); [`LaneEvent::Inspected`] answers.
    Inspect {
        title: String,
        request: String,
        payload: Value,
    },
    /// Poll the session's subagents for the RLM views ([`LaneEvent::RlmStatus`]).
    RlmStatus,
    /// A read-only inspection for the RLM views (`rlm.frames`, `rlm.pool`, `ctx.state`, `instances.list`), answered
    /// by [`LaneEvent::RlmInspected`] with `tag`.
    RlmInspect {
        tag: String,
        request: String,
        payload: Value,
    },
    /// The settings for `/settings` ([`LaneEvent::Settings`]).
    ReadSettings,
    /// The settings' mode for `/mode` and `/engineering` ([`LaneEvent::Mode`]).
    ReadMode,
    /// `/persona` without an argument: the session's persona state, as a [`LaneEvent::Status`].
    ReadPersona,
    /// `/persona on|off|auto`: the session's own persona toggle; its answer arrives as a [`LaneEvent::Status`].
    Persona(String),
    /// `/keepgoing [on|off]`: the session's keep-going mode; its answer arrives as a [`LaneEvent::Status`].
    KeepGoing(String),
    /// `/autoreview`: answered by [`LaneEvent::AutoreviewAccounts`] for [`AutoreviewRequest::Accounts`], else by a
    /// [`LaneEvent::Status`].
    Autoreview(AutoreviewRequest),
    /// The sign-in options for `/login` ([`LaneEvent::LoginProviders`]).
    LoginProviders(String),
    /// Start a subscription (OAuth) login for `provider` (`name` is what the dialog says): [`LaneEvent::Login`]
    /// steps, then statuses and a fresh [`LaneEvent::Catalog`] answer.
    OAuthLogin {
        provider: String,
        name: String,
    },
    /// The answer to the login's pending prompt; `None` cancels the login.
    LoginReply(Option<String>),
    /// Refresh the model catalogs (`models.refresh`): a fresh [`LaneEvent::Catalog`], then
    /// [`LaneEvent::CatalogRefreshed`].
    RefreshCatalog,
    /// Save an API key for a provider; statuses and a fresh [`LaneEvent::Catalog`] answer.
    SaveApiKey {
        provider: String,
        key: String,
    },
    /// Save a credential `ModelRuntime.login` produced (`{type: "api_key", key?, env?}`) for `provider`; answered as
    /// [`LaneCommand::SaveApiKey`] is.
    SaveCredential {
        provider: String,
        credential: Value,
    },
    /// The stored credentials ([`LaneEvent::Credentials`]).
    Credentials,
    /// Remove a stored credential; a [`LaneEvent::Status`] with `done` and a fresh [`LaneEvent::Catalog`] answer.
    Logout {
        provider: String,
        done: String,
    },
    /// Read the changelog ([`LaneEvent::Changelog`]).
    Changelog,
    /// `getChangelogForDisplay` for a session that opened without messages: the entries newer than the
    /// `lastChangelogVersion` setting (which it records), as [`LaneEvent::StartupNotice`] when there are any.
    StartupChangelog,
    /// `maybeWarnAboutAnthropicSubscriptionAuth` for the session's model's provider: a
    /// [`StartupNotice::SubscriptionAuth`] when its credential is a subscription login and the warning is on.
    SubscriptionAuthWarning(String),
    /// Run `/mcp args` (`LegacyExtensionCommands.run`, the built-in MCP gateway's command); statuses answer. Its
    /// dialogs and notices arrive as [`LaneEvent::ExtensionUi`].
    Mcp(String),
    /// `/share`: export the session and upload it as a secret gist with `gh`; statuses answer, and
    /// [`LaneEvent::ShareLoader`] while the gist is created. The page uses `theme`, as `/export` does.
    Share {
        theme: Option<String>,
    },
    /// Esc on the share loader: stop the gist upload; nothing more is reported about it.
    CancelShare,
    /// `/bug`'s summary written by the session's model (`summarizeForBugReport`); [`LaneEvent::BugSummary`] answers.
    BugReportSummary(Option<String>),
    /// Stop the summary in progress; its answer is then ignored.
    AbortBugReportSummary,
    /// Build the report and upload or export it; [`LaneEvent::BugReport`] answers (an upload stopped with
    /// [`LaneCommand::CancelBugReport`] answers nothing).
    BugReport(BugReportRequest),
    /// Export the report the failed upload built as a zip; [`LaneEvent::BugReport`] answers.
    BugReportExport,
    /// Esc on the upload loader: stop the upload.
    CancelBugReport,
    /// Every project's sessions for the resume selector's All scope ([`LaneEvent::AllSessions`]).
    ListAllSessions,
    /// Pi's sessions of every project for `/import`'s All scope ([`LaneEvent::PiAllSessions`]).
    ListAllPiSessions,
    /// Delete a stored session (not the attached one); [`LaneEvent::SessionDeleted`] answers.
    DeleteSession(String),
    /// Rename a stored session (the attached one through [`LaneCommand::SetName`]); [`LaneEvent::Sessions`] answers
    /// with the fresh listing.
    RenameSession {
        path: String,
        name: String,
    },
    /// Abort and release a subagent (and its unfinished descendants).
    KillAgent(String),
    /// Steer a subagent with `text` (`agent_message.send` with `steer: true`); [`LaneEvent::AgentAnswered`] answers.
    SteerAgent {
        id: String,
        text: String,
    },
    /// Start an interrupted subagent again from its brief; [`LaneEvent::AgentAnswered`] answers.
    ReviveAgent(String),
    /// A subagent's transcript ([`LaneEvent::AgentTranscript`]).
    AgentTranscript(String),
    /// Save a global setting (Pi's `SettingsManager` writes).
    SaveSetting {
        key: String,
        value: Value,
    },
    /// The terminal's appearance as the TUI knows it (`"light"` / `"dark"`: OSC 11, a scheme report, else
    /// `COLORFGBG`), told when it is first known and whenever it changes: the session's `diagram` `auto` theme and the
    /// settings lane follow it.
    TerminalTheme(String),
    /// `/trust`: the worker's trust state ([`LaneEvent::Trust`], or a [`LaneEvent::Status`] error).
    ReadTrust,
    /// Save a `/trust` choice through the worker (`setProjectTrust`); a [`LaneEvent::Status`] answers.
    SetProjectTrust {
        trusted: bool,
        updates: Vec<(String, Option<bool>)>,
    },
    /// `/debug`: add the worker's and server's state to what the TUI shows and write the log
    /// ([`LaneEvent::DebugLogWritten`], or a [`LaneEvent::Status`] error).
    WriteDebugLog(crate::debug_log::DebugCapture),
    /// `/reload`: reload the server-selected presentation plugins and the session's plugins; statuses answer.
    ReloadPlugins,
}

impl From<rlm_views::Inspect> for LaneCommand {
    fn from(inspect: rlm_views::Inspect) -> LaneCommand {
        LaneCommand::RlmInspect {
            tag: inspect.tag,
            request: inspect.request.into(),
            payload: inspect.payload,
        }
    }
}

/// The diagnostic the Claude subscription router puts on the message where it switched routes
/// (`ai::claude_subscription::fallback::ROUTE_DIAGNOSTIC`).
pub const ROUTE_DIAGNOSTIC: &str = "claude_subscription_route";

/// The route notice an assistant message carries (`details.notice` of its `claude_subscription_route` diagnostic):
/// "Claude subscription limit reached; using API credits until <time>", or the switch back.
pub fn route_notice(message: &Value) -> Option<String> {
    if message["role"] != "assistant" {
        return None;
    }
    message["diagnostics"]
        .as_array()?
        .iter()
        .find(|d| d["type"] == ROUTE_DIAGNOSTIC || d["kind"] == ROUTE_DIAGNOSTIC)
        .and_then(|d| d["details"]["notice"].as_str())
        .map(str::to_string)
}

/// What `/autoreview` asks of the client (client-tui-pi-commands.ts `setAutoreview`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoreviewRequest {
    /// The setting, the accounts it reviews as and whether the user service is installed.
    Status,
    /// The `gh` accounts to choose from.
    Accounts,
    /// Save `autoreview.enabled: true` and `autoreview.accounts` (`None`: every logged-in account), then write and
    /// start the user service. `reviewing_as` is what the status names.
    On {
        accounts: Option<Vec<String>>,
        reviewing_as: String,
    },
    /// Save `autoreview.enabled: false`, stop the service and remove its file.
    Off,
    /// Add `login` to `autoreview.ban` for `account` (`None`: every account, `*`).
    Ban { login: String, account: Option<String> },
    /// Remove `login` from `autoreview.ban` for `account` (`None`: every account, `*`).
    Unban { login: String, account: Option<String> },
}
