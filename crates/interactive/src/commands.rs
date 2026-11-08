//! The client TUI's slash commands as the editor's autocomplete lists them (the order the TypeScript facets register
//! them in: client-tui-commands.ts, client-tui-pi-commands.ts, the TUI's own, the built-in facet's, the
//! inspection commands and the session worker's), the `/hotkeys` table and the `/session` text.

use std::rc::Rc;

use theme::{Fg, Theme};
use tui::KeybindingsManager;
use tui::markdown_tokens;
use tui::{Component, Spacer, Text};
use tui::{Markdown, MarkdownOptions};

use crate::components::{bold, format_key_text, format_tokens, markdown_theme};
use jsrt::number;

/// Who offers a slash command, which decides where it is listed and when it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// The TypeScript client TUI's own: listed and run.
    Client,
    /// Rust-only, with the overlays ([`crate::qol::QolSettings::overlays`]): a parity build has exactly the client
    /// TUI's commands.
    Overlay,
    /// Pi's interactive mode's (interactive-mode.ts), which the client TUI does not list: run, and listed with the
    /// overlays.
    Pi,
    /// Pi's easter eggs: run, never listed.
    Hidden,
}

/// One command: name, argument hint, description and tier.
pub struct CommandSpec {
    pub name: &'static str,
    pub hint: &'static str,
    pub description: &'static str,
    pub tier: Tier,
}

/// The slash commands this TUI runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slash {
    Name,
    Session,
    Usage,
    Goal,
    Copy,
    Export,
    Hotkeys,
    Changelog,
    Quit,
    Settings,
    Login,
    Logout,
    Share,
    Resume,
    Import,
    Engineering,
    Autoreview,
    Trust,
    ScopedModels,
    Debug,
    New,
    Rlm,
    Tree,
    Fork,
    Clone,
    Model,
    Agents,
    Memory,
    Skills,
    Experiments,
    Goals,
    Progress,
    Extension,
    Mcp,
    Thinking,
    Compact,
    Review,
    Reload,
    Mode,
    Persona,
    KeepGoing,
    Schedules,
    Clear,
    Bug,
    ArminSaysHi,
    DementedElves,
}

impl Slash {
    /// Every command, in the order the editor's autocomplete lists them (the client TUI's, then the Rust-only ones).
    pub const ALL: [Slash; 46] = [
        Slash::Name,
        Slash::Session,
        Slash::Usage,
        Slash::Goal,
        Slash::Copy,
        Slash::Export,
        Slash::Hotkeys,
        Slash::Changelog,
        Slash::Quit,
        Slash::Settings,
        Slash::Login,
        Slash::Logout,
        Slash::Share,
        Slash::Resume,
        Slash::Import,
        Slash::Engineering,
        Slash::Autoreview,
        Slash::Trust,
        Slash::ScopedModels,
        Slash::Debug,
        Slash::New,
        Slash::Rlm,
        Slash::Tree,
        Slash::Fork,
        Slash::Clone,
        Slash::Model,
        Slash::Agents,
        Slash::Memory,
        Slash::Skills,
        Slash::Experiments,
        Slash::Goals,
        Slash::Progress,
        Slash::Extension,
        Slash::Mcp,
        Slash::Thinking,
        Slash::Compact,
        Slash::Review,
        Slash::Reload,
        Slash::Mode,
        Slash::Persona,
        Slash::KeepGoing,
        Slash::Schedules,
        Slash::Clear,
        Slash::Bug,
        Slash::ArminSaysHi,
        Slash::DementedElves,
    ];

    pub fn spec(self) -> CommandSpec {
        let (name, hint, description, tier) = match self {
            Slash::Name => ("name", "<name>", "Set session display name", Tier::Client),
            Slash::Session => ("session", "", "Show session info and stats", Tier::Client),
            Slash::Usage => (
                "usage",
                "",
                "What this session did: depth (frames, sub-agents), cells, tokens and cost, guardrails",
                Tier::Client,
            ),
            Slash::Goal => (
                "goal",
                "[objective | pause | resume | clear | check <command>]",
                "Set a goal the agent works on round after round (pause, resume, clear, check <command>)",
                Tier::Client,
            ),
            Slash::Copy => ("copy", "", "Copy last agent message to clipboard", Tier::Client),
            Slash::Export => (
                "export",
                "[path]",
                "Export session (HTML default, or specify path: .html/.jsonl)",
                Tier::Client,
            ),
            Slash::Hotkeys => ("hotkeys", "", "Show all keyboard shortcuts", Tier::Client),
            Slash::Changelog => ("changelog", "", "Show changelog entries", Tier::Client),
            Slash::Quit => ("quit", "", "Quit ultron", Tier::Client),
            Slash::Settings => ("settings", "", "Open settings menu", Tier::Client),
            Slash::Login => ("login", "<provider>", "Configure provider authentication", Tier::Client),
            Slash::Logout => ("logout", "", "Remove provider authentication", Tier::Client),
            Slash::Share => ("share", "", "Share session as a secret GitHub gist", Tier::Client),
            Slash::Resume => ("resume", "", "Resume a different session", Tier::Client),
            Slash::Import => (
                "import",
                "[path.jsonl]",
                "Import a Pi session (JSONL) as a native session",
                Tier::Client,
            ),
            Slash::Engineering => (
                "engineering",
                "[true|false]",
                "Engineering mode: put the always-on skills (alwaysSkills) in the system prompt",
                Tier::Client,
            ),
            Slash::Autoreview => (
                "autoreview",
                "[on|off]",
                "Automatic pull-request reviews as your gh account: on (picks the account, starts the service), off, or the status",
                Tier::Client,
            ),
            Slash::Trust => (
                "trust",
                "",
                "Save project trust decision for future sessions",
                Tier::Client,
            ),
            Slash::ScopedModels => (
                "scoped-models",
                "",
                "Enable/disable models for Ctrl+P cycling",
                Tier::Client,
            ),
            Slash::Debug => (
                "debug",
                "",
                "Write a debug log (TUI, Session worker and server state)",
                Tier::Client,
            ),
            Slash::New => ("new", "", "Start a new session", Tier::Client),
            Slash::Rlm => (
                "rlm",
                "",
                "Toggle the live RLM graph panel; /rlm focus opens it full screen, /rlm pane the RLM pane",
                Tier::Client,
            ),
            Slash::Tree => ("tree", "", "Navigate the session tree (switch branches)", Tier::Client),
            Slash::Fork => (
                "fork",
                "",
                "Create a new fork from a previous user message",
                Tier::Client,
            ),
            Slash::Clone => (
                "clone",
                "",
                "Duplicate the current session at the current position",
                Tier::Client,
            ),
            Slash::Model => ("model", "<provider/model>", "Select model", Tier::Client),
            Slash::Agents => (
                "agents",
                "[task-id]",
                "Show Ultron tasks, definitions, and usage",
                Tier::Client,
            ),
            Slash::Memory => (
                "memory",
                "why <task-id> | list",
                "Show recorded memory evidence without a new search",
                Tier::Client,
            ),
            Slash::Skills => (
                "skills",
                "why <decision-id> | list",
                "Show skill versions or why a skill was selected",
                Tier::Client,
            ),
            Slash::Experiments => ("experiments", "", "Show recorded experiment attempts", Tier::Client),
            Slash::Goals => (
                "goals",
                "[goal-id]",
                "Show goals and their verification state",
                Tier::Client,
            ),
            Slash::Progress => (
                "progress",
                "<task-id>",
                "Show progress receipts and decisions for a task",
                Tier::Client,
            ),
            // TypeScript's `/extension <name> [args]`: kept in the list (as the client TUI lists it); Pi extensions are not
            // supported by ultron-rs, so it says so.
            Slash::Extension => (
                "extension",
                "<name> [args]",
                "Run a loaded Pi extension command",
                Tier::Client,
            ),
            Slash::Mcp => (
                "mcp",
                "",
                "MCP servers: status, connect or disconnect, sign in, enable or disable",
                Tier::Client,
            ),
            Slash::Thinking => ("thinking", "<level>", "Set thinking level", Tier::Client),
            Slash::Compact => (
                "compact",
                "<instructions>",
                "Manually compact the session context",
                Tier::Client,
            ),
            Slash::Review => (
                "review",
                "[base-ref|PR|path...] [--only bugs,security,arch,tests,ai] [--budget 300k] [--model p/m] [--post]",
                "Review code changes: specialist reviewer frames, then a verifier per finding",
                Tier::Client,
            ),
            Slash::Reload => ("reload", "", "Reload server-selected plugins", Tier::Client),
            Slash::Mode => (
                "mode",
                "[name]",
                "Switch mode (Default, Engineering); without a name, pick one",
                Tier::Overlay,
            ),
            Slash::Persona => (
                "persona",
                "[on|off|auto|ultron|none]",
                "The Ultron persona: on/off/auto for this session, ultron/none for the setting",
                Tier::Overlay,
            ),
            Slash::KeepGoing => (
                "keepgoing",
                "[on|off]",
                "Keep-going mode: continue every working turn until the work is done (while you are away)",
                Tier::Overlay,
            ),
            Slash::Schedules => (
                "schedules",
                "",
                "Scheduled prompts of this session (schedule.every / schedule.at): name, next run, runs",
                Tier::Overlay,
            ),
            // Claude Code's `/clear`: `/new` by another name.
            Slash::Clear => (
                "clear",
                "",
                "Clear the conversation: start a new session",
                Tier::Overlay,
            ),
            // Pi's own (core/slash-commands.ts), which the client TUI does not offer.
            Slash::Bug => ("bug", "<description>", "Report a bug to the Pi developers", Tier::Pi),
            Slash::ArminSaysHi => ("arminsayshi", "", "", Tier::Hidden),
            Slash::DementedElves => ("dementedelves", "", "", Tier::Hidden),
        };
        CommandSpec {
            name,
            hint,
            description,
            tier,
        }
    }

    /// The command `/name` runs, with the overlays on or off; `None` when there is none.
    pub fn parse(name: &str, overlays: bool) -> Option<Slash> {
        Slash::ALL
            .into_iter()
            .find(|c| c.spec().name == name)
            .filter(|c| c.spec().tier != Tier::Overlay || overlays)
    }

    /// Whether the editor's autocomplete lists the command.
    pub fn listed(self, overlays: bool) -> bool {
        match self.spec().tier {
            Tier::Client => true,
            Tier::Overlay | Tier::Pi => overlays,
            Tier::Hidden => false,
        }
    }
}

/// `keyDisplayText(keybinding)`: the binding's keys, capitalized.
pub fn key_display_text(keys: &KeybindingsManager, keybinding: &str) -> String {
    format_key_text(&keys.get_keys(keybinding).join("/"), true, false)
}

/// `hotkeysMarkdown()`.
pub fn hotkeys_markdown(keys: &KeybindingsManager) -> String {
    let k = |id: &str| key_display_text(keys, id);
    format!(
        "
**Navigation**
| Key | Action |
|-----|--------|
| `{}` / `{}` | Move cursor / browse prompt history |
| `{}` | Reverse-search prompt history |
| `{}` / `{}` | Move by word |
| `{}` | Start of line |
| `{}` | End of line |
| `{}` | Jump forward to character |
| `{}` | Jump backward to character |
| `{}` / `{}` | Scroll by page |

**Editing**
| Key | Action |
|-----|--------|
| `{}` | Send message (steers while a turn runs) |
| `{}` | New line |
| `{}` | Delete word backwards |
| `{}` | Delete word forwards |
| `{}` | Delete to start of line |
| `{}` | Delete to end of line |
| `{}` | Paste the most-recently-deleted text |
| `{}` | Cycle through the deleted text after pasting |
| `{}` | Undo |

**Other**
| Key | Action |
|-----|--------|
| `{}` | Path completion / accept autocomplete |
| `{}` | Cancel autocomplete / abort the turn |
| `{}` | Clear editor (first) / exit (second) |
| `{}` | Exit (when editor is empty) |
| `{}` | Suspend to background |
| `{}` | Cycle thinking level |
| `{}` / `{}` | Cycle models |
| `{}` | Open model selector |
| `{}` | Toggle tool output expansion |
| `{}` | Toggle thinking block visibility |
| `{}` | Edit message in external editor |
| `{}` | Copy last assistant message |
| `{}` | Queue follow-up message |
| `{}` | Restore queued messages |
| `{}` | Paste image or text from clipboard |
| `{}` | Toggle the live RLM panel |
| `{}` | Open the full-screen RLM graph |
| `{}` | Open the RLM pane beside the chat, or move the focus between it and the chat |
| `/` | Slash commands |
| `/usage` | What this session did: depth (frames, sub-agents), cells, tokens and cost, guardrails |
| `!` | Run bash command |
| `!!` | Run bash command (excluded from context) |
",
        k("tui.editor.cursorUp"),
        k("tui.editor.cursorDown"),
        k("app.history.search"),
        k("tui.editor.cursorWordLeft"),
        k("tui.editor.cursorWordRight"),
        k("tui.editor.cursorLineStart"),
        k("tui.editor.cursorLineEnd"),
        k("tui.editor.jumpForward"),
        k("tui.editor.jumpBackward"),
        k("tui.editor.pageUp"),
        k("tui.editor.pageDown"),
        k("tui.input.submit"),
        k("tui.input.newLine"),
        k("tui.editor.deleteWordBackward"),
        k("tui.editor.deleteWordForward"),
        k("tui.editor.deleteToLineStart"),
        k("tui.editor.deleteToLineEnd"),
        k("tui.editor.yank"),
        k("tui.editor.yankPop"),
        k("tui.editor.undo"),
        k("tui.input.tab"),
        k("app.interrupt"),
        k("app.clear"),
        k("app.exit"),
        k("app.suspend"),
        k("app.thinking.cycle"),
        k("app.model.cycleForward"),
        k("app.model.cycleBackward"),
        k("app.model.select"),
        k("app.tools.expand"),
        k("app.thinking.toggle"),
        k("app.editor.external"),
        k("app.message.copy"),
        k("app.message.followUp"),
        k("app.message.dequeue"),
        k("app.clipboard.pasteImage"),
        k("app.rlm.toggle"),
        k("app.rlm.focus"),
        k("app.rlm.pane"),
    )
}

/// `markdownNotice(title, markdown)`: a bordered titled Markdown block.
pub struct MarkdownNotice {
    pub theme: Rc<Theme>,
    pub title: String,
    pub markdown: String,
}

impl Component for MarkdownNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        notice_lines(&self.theme, &self.title, markdown_tokens(self.markdown.trim()), width)
    }
}

/// A [`MarkdownNotice`] whose Markdown was lexed already (`/changelog`, lexed off the UI thread).
pub struct LexedNotice {
    pub theme: Rc<Theme>,
    pub title: String,
    pub tokens: Vec<tui::Token>,
}

impl Component for LexedNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        notice_lines(&self.theme, &self.title, self.tokens.clone(), width)
    }
}

fn notice_lines(t: &Rc<Theme>, title: &str, tokens: Vec<tui::Token>, width: usize) -> Vec<String> {
    let border = t.fg(Fg::Border, &"─".repeat(width.max(1)));
    let mut lines = vec![border.clone()];
    lines.extend(Text::new(&bold(&t.fg(Fg::Accent, title)), 1, 0, None).render(width));
    lines.extend(Spacer(1).render(width));
    let mut md = Markdown {
        tokens,
        padding_x: 1,
        padding_y: 1,
        theme: markdown_theme(t),
        style: None,
        options: MarkdownOptions {
            hyperlinks: crate::render_settings::hyperlinks(),
            ..MarkdownOptions::default()
        },
    };
    lines.extend(md.render(width));
    lines.push(border);
    lines
}

/// What `/session` reports (`SessionStats` over the session's entries).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub session_file: Option<String>,
    pub session_id: String,
    pub name: Option<String>,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// (tokens, context window, percent) when the model's window is known.
    pub context: Option<(Option<u64>, u64, Option<f64>)>,
    /// Runs the persona's auto-nudge continued (its `ultron.nudge` messages).
    pub auto_continues: usize,
}

/// `toLocaleString()` for an integer in the en-US locale.
fn locale(n: u64) -> String {
    crate::components::group_thousands(n)
}

/// Pi's `/session` text.
pub fn session_info(t: &Theme, s: &SessionStats) -> String {
    let dim = |x: &str| t.fg(Fg::Dim, x);
    let mut info = format!("{}\n\n", bold("Session Info"));
    if let Some(name) = &s.name {
        info += &format!("{} {name}\n", dim("Name:"));
    }
    info += &format!(
        "{} {}\n",
        dim("File:"),
        s.session_file.as_deref().unwrap_or("In-memory")
    );
    info += &format!("{} {}\n\n", dim("ID:"), s.session_id);
    info += &format!("{}\n", bold("Messages"));
    info += &format!(
        "{} {}\n",
        dim("Total:"),
        s.user_messages + s.assistant_messages + s.tool_results
    );
    info += &format!("{} {}\n", dim("User:"), s.user_messages);
    info += &format!("{} {}\n", dim("Assistant:"), s.assistant_messages);
    info += &format!("{} {} calls, {} results\n", dim("Tools:"), s.tool_calls, s.tool_results);
    // Rust-only (the persona's auto-nudge), so only when there was one.
    if s.auto_continues > 0 {
        info += &format!("{} {}\n", dim("Auto-continues:"), s.auto_continues);
    }
    info += "\n";
    info += &format!("{}\n", bold("Tokens"));
    let prompt = s.input + s.cache_read + s.cache_write;
    info += &format!("{} {}\n", dim("Input:"), locale(prompt));
    if prompt > 0 && (s.cache_read > 0 || s.cache_write > 0) {
        let rate = dim(&format!(
            "({}%)",
            number::to_fixed(s.cache_read as f64 / prompt as f64 * 100.0, 1)
        ));
        info += &format!("  {} {} {rate}\n", dim("Cached:"), locale(s.cache_read));
        info += &format!("  {} {}\n", dim("Uncached:"), locale(s.input + s.cache_write));
    }
    info += &format!("{} {}\n", dim("Output:"), locale(s.output));
    info += &format!(
        "{} {}\n",
        dim("Total:"),
        locale(s.input + s.output + s.cache_read + s.cache_write)
    );
    if let Some((tokens, window, percent)) = s.context {
        let used = tokens.map_or("?".to_string(), format_tokens);
        let percent = percent.map_or(String::new(), |p| format!(" ({}%)", number::to_fixed(p, 1)));
        info += &format!(
            "\n{}\n{} {used}/{}{percent}\n",
            bold("Context"),
            dim("Used:"),
            format_tokens(window)
        );
    }
    if s.cost > 0.0 {
        info += &format!("\n{}\n{} ${}", bold("Cost"), dim("Total:"), number::to_fixed(s.cost, 3));
    }
    info.trim_end().to_string()
}
