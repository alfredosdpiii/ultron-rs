//! The app's keybindings: Ultron's `KEYBINDINGS` (packages/coding-agent/src/core/keybindings.ts) over the TUI's,
//! and the keybinding ids of the omp-inspired overlays (TUI QoL batch 2): retry, display reset, the agent hub, the
//! `/copy` and Esc-Esc rewind pickers, the model hub, prompt-history search and the transcript browser.
//!
//! `app_keybindings` stays a byte-for-byte port of Ultron's TypeScript `KEYBINDINGS` (its fixture test compares
//! the whole list), so these ids live here and the composition root resolves [`native_app_keybindings`]. Every
//! overlay matches keys through a [`tui::KeybindingsManager`] built from it, never with literal key
//! checks, so a user override in `<agentDir>/keybindings.json` (loaded by the composition root through
//! [`tui::keybindings_file`], and again on `/reload`) reaches the overlays too.
//!
//! Global ids reuse omp's names and defaults (oh-my-pi packages/tui/src/app-keybindings.ts) except where a key is
//! already Ultron's: omp binds `app.retry` to `["f5", "alt+r"]`, but `alt+r` is Ultron's `app.history.search`
//! (omp searches history with `ctrl+r`, which Ultron gives to `app.rlm.toggle`). Retry keeps omp's leading key,
//! F5, which every terminal delivers verbatim.

use std::time::{Duration, Instant};

use tui::{KeybindingDefinition, tui_keybindings};

/// Platform facts the TS module reads from `process.platform` and the WSL environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Platform {
    pub win32: bool,
    pub darwin: bool,
    /// `useWindowsKeybindings()`: win32, or linux under WSL.
    pub windows_keybindings: bool,
}

impl Platform {
    pub fn new(platform: &str, wsl: bool) -> Self {
        Platform {
            win32: platform == "win32",
            darwin: platform == "darwin",
            windows_keybindings: platform == "win32" || (platform == "linux" && wsl),
        }
    }
}

/// `KEYBINDINGS` for a platform: the TUI bindings (some overridden in place), then the app bindings.
pub fn app_keybindings(p: Platform) -> Vec<KeybindingDefinition> {
    let overrides: [(&str, &[&str]); 4] = [
        (
            "tui.editor.undo",
            if p.win32 {
                &["ctrl+z"]
            } else if p.windows_keybindings {
                &["alt+z"]
            } else {
                &["ctrl+-"]
            },
        ),
        (
            "tui.altScreen.previousPrompt",
            if p.windows_keybindings {
                &["ctrl+up"]
            } else {
                &["ctrl+shift+up", "ctrl+up"]
            },
        ),
        (
            "tui.altScreen.nextPrompt",
            if p.windows_keybindings {
                &["ctrl+down"]
            } else {
                &["ctrl+shift+down", "ctrl+down"]
            },
        ),
        (
            "tui.altScreen.search",
            if p.windows_keybindings {
                &["ctrl+f"]
            } else {
                &["ctrl+shift+f"]
            },
        ),
    ];
    let mut defs = tui_keybindings();
    for (id, keys) in overrides {
        if let Some(d) = defs.iter_mut().find(|d| d.id == id) {
            d.default_keys = keys.iter().map(|k| (*k).to_string()).collect();
        }
    }
    defs.extend([
        KeybindingDefinition::new("app.interrupt", &["escape"], "Cancel or abort"),
        KeybindingDefinition::new("app.clear", &["ctrl+c"], "Clear editor"),
        KeybindingDefinition::new("app.exit", &["ctrl+d"], "Exit when editor is empty"),
        KeybindingDefinition::new(
            "app.suspend",
            if p.win32 { &[] } else { &["ctrl+z"] },
            "Suspend to background",
        ),
        KeybindingDefinition::new("app.thinking.cycle", &["shift+tab"], "Cycle thinking level"),
        KeybindingDefinition::new("app.thinking.save", &["ctrl+s"], "Save thinking level"),
        KeybindingDefinition::new("app.model.cycleForward", &["ctrl+p"], "Cycle to next model"),
        KeybindingDefinition::new(
            "app.model.cycleBackward",
            if p.windows_keybindings {
                &["alt+p"]
            } else {
                &["shift+ctrl+p"]
            },
            "Cycle to previous model",
        ),
        KeybindingDefinition::new("app.model.select", &["ctrl+l"], "Open model selector"),
        KeybindingDefinition::new("app.tools.expand", &["ctrl+o"], "Toggle tool output"),
        KeybindingDefinition::new("app.thinking.toggle", &["ctrl+t"], "Toggle thinking blocks"),
        KeybindingDefinition::new("app.rlm.toggle", &["ctrl+r"], "Toggle the live RLM panel (native TUI)"),
        KeybindingDefinition::new(
            "app.rlm.focus",
            &["alt+g"],
            "Open the full-screen RLM graph (native TUI)",
        ),
        KeybindingDefinition::new("app.rlm.graph.up", &["up", "k"], "RLM graph: select the previous node"),
        KeybindingDefinition::new("app.rlm.graph.down", &["down", "j"], "RLM graph: select the next node"),
        KeybindingDefinition::new("app.rlm.graph.pageUp", &["pageUp"], "RLM graph: page up"),
        KeybindingDefinition::new("app.rlm.graph.pageDown", &["pageDown"], "RLM graph: page down"),
        KeybindingDefinition::new(
            "app.rlm.graph.details",
            &["enter"],
            "RLM graph: show or hide the node's details",
        ),
        KeybindingDefinition::new(
            "app.rlm.graph.collapse",
            &["c", "space"],
            "RLM graph: collapse or expand the subtree",
        ),
        KeybindingDefinition::new(
            "app.rlm.graph.exit",
            &["escape", "q"],
            "RLM graph: leave the focus view",
        ),
        KeybindingDefinition::new(
            "app.rlm.pane",
            &["alt+w"],
            "Open the RLM pane, or move focus between it and the chat (native TUI)",
        ),
        KeybindingDefinition::new("app.rlm.pane.next", &["tab", "n"], "RLM pane: select the next node"),
        KeybindingDefinition::new(
            "app.rlm.pane.prev",
            &["shift+tab", "p"],
            "RLM pane: select the previous node",
        ),
        KeybindingDefinition::new(
            "app.rlm.pane.fold",
            &["space", "enter"],
            "RLM pane: fold or unfold the node's box",
        ),
        KeybindingDefinition::new(
            "app.rlm.pane.details",
            &["d"],
            "RLM pane: show or hide the node detail cards",
        ),
        KeybindingDefinition::new("app.rlm.pane.scrollUp", &["up"], "RLM pane: scroll up"),
        KeybindingDefinition::new("app.rlm.pane.scrollDown", &["down"], "RLM pane: scroll down"),
        KeybindingDefinition::new("app.rlm.pane.pageUp", &["pageUp"], "RLM pane: page up"),
        KeybindingDefinition::new("app.rlm.pane.pageDown", &["pageDown"], "RLM pane: page down"),
        KeybindingDefinition::new("app.rlm.pane.prevRun", &["left"], "RLM pane: previous run"),
        KeybindingDefinition::new("app.rlm.pane.nextRun", &["right"], "RLM pane: next run"),
        KeybindingDefinition::new("app.rlm.pane.close", &["q", "escape"], "RLM pane: close the pane"),
        KeybindingDefinition::new(
            "app.history.search",
            &["alt+r"],
            "Reverse-search prompt history (native TUI)",
        ),
        KeybindingDefinition::new(
            "app.session.toggleNamedFilter",
            &["ctrl+n"],
            "Toggle named session filter",
        ),
        KeybindingDefinition::new("app.editor.external", &["ctrl+g"], "Open external editor"),
        KeybindingDefinition::new(
            "app.message.copy",
            &["ctrl+x"],
            "Copy selection or last assistant message",
        ),
        KeybindingDefinition::new(
            "app.message.followUp",
            if p.windows_keybindings {
                &["ctrl+q"]
            } else {
                &["alt+enter"]
            },
            "Queue follow-up message",
        ),
        KeybindingDefinition::new(
            "app.message.dequeue",
            if p.windows_keybindings { &["alt+q"] } else { &["alt+up"] },
            "Restore queued messages",
        ),
        KeybindingDefinition::new(
            "app.clipboard.pasteImage",
            if p.windows_keybindings { &["alt+v"] } else { &["ctrl+v"] },
            "Paste image from clipboard (text fallback)",
        ),
        KeybindingDefinition::new("app.session.new", &[], "Start a new session"),
        KeybindingDefinition::new("app.session.tree", &[], "Open session tree"),
        KeybindingDefinition::new("app.session.fork", &[], "Fork current session"),
        KeybindingDefinition::new("app.session.resume", &[], "Resume a session"),
        KeybindingDefinition::new(
            "app.tree.foldOrUp",
            if p.darwin {
                &["alt+left", "ctrl+left"]
            } else {
                &["ctrl+left", "alt+left"]
            },
            "Fold tree branch or move up",
        ),
        KeybindingDefinition::new(
            "app.tree.unfoldOrDown",
            if p.darwin {
                &["alt+right", "ctrl+right"]
            } else {
                &["ctrl+right", "alt+right"]
            },
            "Unfold tree branch or move down",
        ),
        KeybindingDefinition::new("app.tree.editLabel", &["shift+l"], "Edit tree label"),
        KeybindingDefinition::new(
            "app.tree.toggleLabelTimestamp",
            &["shift+t"],
            "Toggle tree label timestamps",
        ),
        KeybindingDefinition::new("app.session.togglePath", &["ctrl+p"], "Toggle session path display"),
        KeybindingDefinition::new("app.session.toggleSort", &["ctrl+s"], "Toggle session sort mode"),
        KeybindingDefinition::new("app.session.rename", &["ctrl+r"], "Rename session"),
        KeybindingDefinition::new("app.session.delete", &["ctrl+d"], "Delete session"),
        KeybindingDefinition::new(
            "app.session.deleteNoninvasive",
            &["ctrl+backspace"],
            "Delete session when query is empty",
        ),
        KeybindingDefinition::new("app.models.save", &["ctrl+s"], "Save model selection"),
        KeybindingDefinition::new("app.models.enableAll", &["ctrl+a"], "Enable all models"),
        KeybindingDefinition::new("app.models.clearAll", &["ctrl+x"], "Clear all models"),
        KeybindingDefinition::new(
            "app.models.toggleProvider",
            &["ctrl+p"],
            "Toggle all models for provider",
        ),
        KeybindingDefinition::new("app.models.reorderUp", &["alt+up"], "Move model up in order"),
        KeybindingDefinition::new("app.models.reorderDown", &["alt+down"], "Move model down in order"),
        KeybindingDefinition::new("app.tree.filter.default", &["ctrl+d"], "Tree filter: default view"),
        KeybindingDefinition::new("app.tree.filter.noTools", &["ctrl+t"], "Tree filter: hide tool results"),
        KeybindingDefinition::new(
            "app.tree.filter.userOnly",
            &["ctrl+u"],
            "Tree filter: user messages only",
        ),
        KeybindingDefinition::new(
            "app.tree.filter.labeledOnly",
            &["ctrl+l"],
            "Tree filter: labeled entries only",
        ),
        KeybindingDefinition::new("app.tree.filter.all", &["ctrl+a"], "Tree filter: show all entries"),
        KeybindingDefinition::new(
            "app.tree.filter.cycleForward",
            &["ctrl+o"],
            "Tree filter: cycle forward",
        ),
        KeybindingDefinition::new(
            "app.tree.filter.cycleBackward",
            &["shift+ctrl+o"],
            "Tree filter: cycle backward",
        ),
    ]);
    defs
}

/// The batch-2 bindings, in display order.
pub fn qol_keybindings() -> Vec<KeybindingDefinition> {
    vec![
        KeybindingDefinition::new("app.retry", &["f5"], "Retry last failed assistant turn"),
        KeybindingDefinition::new("app.display.reset", &["alt+l"], "Reset terminal display"),
        KeybindingDefinition::new("app.agents.hub", &["alt+a"], "Open the agent hub"),
        // Fullscreen transcript browsing shared by /copy, rewind and the agent transcript viewer.
        KeybindingDefinition::new("app.transcript.scrollUp", &["shift+up"], "Transcript: scroll up fast"),
        KeybindingDefinition::new(
            "app.transcript.scrollDown",
            &["shift+down"],
            "Transcript: scroll down fast",
        ),
        // /copy picker.
        KeybindingDefinition::new(
            "app.copy.blocks",
            &["right"],
            "Copy picker: step into the turn's blocks",
        ),
        KeybindingDefinition::new("app.copy.back", &["left"], "Copy picker: back to the transcript"),
        KeybindingDefinition::new(
            "app.copy.open",
            &["o", "shift+o"],
            "Copy picker: open the selected link",
        ),
        // Esc-Esc rewind selector.
        KeybindingDefinition::new("app.rewind.prevUser", &["left"], "Rewind: previous user turn"),
        KeybindingDefinition::new("app.rewind.nextUser", &["right"], "Rewind: next user turn"),
        KeybindingDefinition::new("app.rewind.filter", &["f"], "Rewind: filter turns by words"),
        // Working-tree checkpoints (Rust-only): restore the files to a turn's start.
        KeybindingDefinition::new(
            "app.rewind.restoreFiles",
            &["r"],
            "Rewind: restore files to before this turn",
        ),
        KeybindingDefinition::new(
            "app.tree.restoreFiles",
            &["shift+r"],
            "Tree: restore files to this point",
        ),
        // Agent hub table and transcript viewer.
        KeybindingDefinition::new("app.agentHub.up", &["k"], "Agent hub: previous agent"),
        KeybindingDefinition::new("app.agentHub.down", &["j"], "Agent hub: next agent"),
        KeybindingDefinition::new("app.agentHub.filter", &["/"], "Agent hub: filter agents"),
        KeybindingDefinition::new(
            "app.agentHub.tree",
            &["t"],
            "Agent hub: flat roster or grouped by parent",
        ),
        KeybindingDefinition::new("app.agentHub.revive", &["r"], "Agent hub: revive a parked agent"),
        KeybindingDefinition::new("app.agentHub.kill", &["x"], "Agent hub: abort and release an agent"),
        KeybindingDefinition::new(
            "app.agentHub.close",
            &["escape"],
            "Agent hub: close (or leave the transcript)",
        ),
        KeybindingDefinition::new("app.agentHub.top", &["g"], "Agent transcript: scroll to the top"),
        KeybindingDefinition::new(
            "app.agentHub.bottom",
            &["shift+g"],
            "Agent transcript: scroll to the bottom",
        ),
        // Model hub.
        KeybindingDefinition::new(
            "app.modelHub.focus",
            &["tab"],
            "Model hub: move focus between scopes and models",
        ),
        KeybindingDefinition::new("app.modelHub.scopes", &["left"], "Model hub: focus the scopes sidebar"),
        KeybindingDefinition::new("app.modelHub.models", &["right"], "Model hub: focus the model list"),
        // Prompt-history search (prompt-history-search.ts: the search key and Ctrl+R step older, Ctrl+S newer).
        KeybindingDefinition::new("app.historySearch.older", &["ctrl+r"], "History search: older match"),
        KeybindingDefinition::new("app.historySearch.newer", &["ctrl+s"], "History search: newer match"),
    ]
}

/// What the native TUI resolves: the TypeScript `KEYBINDINGS` followed by [`qol_keybindings`].
pub fn native_app_keybindings(p: Platform) -> Vec<KeybindingDefinition> {
    let mut defs = app_keybindings(p);
    defs.extend(qol_keybindings());
    defs
}

/// omp's Esc-Esc detector: a second tap within the window fires once (a third tap starts over).
#[derive(Clone, Copy, Debug)]
pub struct DoubleTap {
    window: Duration,
    last: Option<Instant>,
}

/// omp's `doubleEscapeAction` window.
pub const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);

impl Default for DoubleTap {
    fn default() -> Self {
        Self::new(DOUBLE_TAP_WINDOW)
    }
}

impl DoubleTap {
    pub fn new(window: Duration) -> Self {
        DoubleTap { window, last: None }
    }

    /// Record a tap at `now`; true when it completes a double tap.
    pub fn tap(&mut self, now: Instant) -> bool {
        match self.last {
            Some(prev) if now.saturating_duration_since(prev) <= self.window => {
                self.last = None;
                true
            }
            _ => {
                self.last = Some(now);
                false
            }
        }
    }

    /// Any other key breaks the sequence.
    pub fn reset(&mut self) {
        self.last = None;
    }
}
