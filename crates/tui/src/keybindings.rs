//! Keybinding registry (port of packages/tui/src/keybindings.ts) and the default editor bindings
//! (`TUI_KEYBINDINGS`).

use std::cell::Cell;
use std::rc::Rc;

use crate::keys::{KeyContext, matches_key};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub id: &'static str,
    pub default_keys: Vec<String>,
    pub description: &'static str,
}

impl KeybindingDefinition {
    pub fn new(id: &'static str, keys: &[&str], description: &'static str) -> Self {
        KeybindingDefinition {
            id,
            default_keys: keys.iter().map(|k| (*k).to_string()).collect(),
            description,
        }
    }
}

/// `TUI_KEYBINDINGS`, in declaration order (the default editor bindings).
pub fn tui_keybindings() -> Vec<KeybindingDefinition> {
    vec![
        KeybindingDefinition::new("tui.editor.cursorUp", &["up"], "Move cursor up"),
        KeybindingDefinition::new("tui.editor.cursorDown", &["down"], "Move cursor down"),
        KeybindingDefinition::new(
            "tui.editor.historyPrevious",
            &[],
            "Select previous prompt history entry",
        ),
        KeybindingDefinition::new("tui.editor.historyNext", &[], "Select next prompt history entry"),
        KeybindingDefinition::new("tui.editor.cursorLeft", &["left", "ctrl+b"], "Move cursor left"),
        KeybindingDefinition::new("tui.editor.cursorRight", &["right", "ctrl+f"], "Move cursor right"),
        KeybindingDefinition::new(
            "tui.editor.cursorWordLeft",
            &["alt+left", "ctrl+left", "alt+b"],
            "Move cursor word left",
        ),
        KeybindingDefinition::new(
            "tui.editor.cursorWordRight",
            &["alt+right", "ctrl+right", "alt+f"],
            "Move cursor word right",
        ),
        KeybindingDefinition::new(
            "tui.editor.cursorLineStart",
            &["home", "ctrl+home", "ctrl+a"],
            "Move to line start",
        ),
        KeybindingDefinition::new(
            "tui.editor.cursorLineEnd",
            &["end", "ctrl+end", "ctrl+e"],
            "Move to line end",
        ),
        KeybindingDefinition::new("tui.editor.jumpForward", &["ctrl+]"], "Jump forward to character"),
        KeybindingDefinition::new("tui.editor.jumpBackward", &["ctrl+alt+]"], "Jump backward to character"),
        KeybindingDefinition::new("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
        KeybindingDefinition::new("tui.editor.pageDown", &["pageDown", "ctrl+pageDown"], "Page down"),
        KeybindingDefinition::new(
            "tui.editor.deleteCharBackward",
            &["backspace"],
            "Delete character backward",
        ),
        KeybindingDefinition::new(
            "tui.editor.deleteCharForward",
            &["delete", "ctrl+d"],
            "Delete character forward",
        ),
        KeybindingDefinition::new(
            "tui.editor.deleteWordBackward",
            &["ctrl+w", "alt+backspace"],
            "Delete word backward",
        ),
        KeybindingDefinition::new(
            "tui.editor.deleteWordForward",
            &["alt+d", "alt+delete"],
            "Delete word forward",
        ),
        KeybindingDefinition::new("tui.editor.deleteToLineStart", &["ctrl+u"], "Delete to line start"),
        KeybindingDefinition::new("tui.editor.deleteToLineEnd", &["ctrl+k"], "Delete to line end"),
        KeybindingDefinition::new("tui.editor.yank", &["ctrl+y"], "Yank"),
        KeybindingDefinition::new("tui.editor.yankPop", &["alt+y"], "Yank pop"),
        KeybindingDefinition::new("tui.editor.undo", &["ctrl+-"], "Undo"),
        KeybindingDefinition::new("tui.input.newLine", &["shift+enter", "ctrl+j"], "Insert newline"),
        KeybindingDefinition::new("tui.input.submit", &["enter"], "Submit input"),
        KeybindingDefinition::new("tui.input.tab", &["tab"], "Tab / autocomplete"),
        KeybindingDefinition::new("tui.input.copy", &["ctrl+c"], "Copy selection"),
        KeybindingDefinition::new("tui.select.up", &["up"], "Move selection up"),
        KeybindingDefinition::new("tui.select.down", &["down"], "Move selection down"),
        KeybindingDefinition::new("tui.select.pageUp", &["pageUp"], "Selection page up"),
        KeybindingDefinition::new("tui.select.pageDown", &["pageDown"], "Selection page down"),
        KeybindingDefinition::new("tui.select.confirm", &["enter"], "Confirm selection"),
        KeybindingDefinition::new("tui.select.cancel", &["escape", "ctrl+c"], "Cancel selection"),
        KeybindingDefinition::new("tui.altScreen.pageUp", &["pageUp"], "Scroll viewport up one page"),
        KeybindingDefinition::new("tui.altScreen.pageDown", &["pageDown"], "Scroll viewport down one page"),
        KeybindingDefinition::new("tui.altScreen.halfPageUp", &[], "Scroll viewport up half a page"),
        KeybindingDefinition::new("tui.altScreen.halfPageDown", &[], "Scroll viewport down half a page"),
        KeybindingDefinition::new("tui.altScreen.lineUp", &[], "Scroll viewport up one line"),
        KeybindingDefinition::new("tui.altScreen.lineDown", &[], "Scroll viewport down one line"),
        KeybindingDefinition::new(
            "tui.altScreen.previousPrompt",
            &["ctrl+shift+up", "ctrl+up"],
            "Jump to previous semantic prompt",
        ),
        KeybindingDefinition::new(
            "tui.altScreen.nextPrompt",
            &["ctrl+shift+down", "ctrl+down"],
            "Jump to next semantic prompt",
        ),
        KeybindingDefinition::new(
            "tui.altScreen.search",
            &["ctrl+shift+f"],
            "Search the primary scroll view",
        ),
        KeybindingDefinition::new(
            "tui.altScreen.searchNext",
            &["enter", "ctrl+g"],
            "Select the next search match",
        ),
        KeybindingDefinition::new(
            "tui.altScreen.searchPrevious",
            &["shift+enter", "ctrl+shift+g"],
            "Select the previous search match",
        ),
        KeybindingDefinition::new("tui.altScreen.searchClose", &["escape"], "Close transcript search"),
        KeybindingDefinition::new("tui.altScreen.top", &["home"], "Scroll viewport to top"),
        KeybindingDefinition::new("tui.altScreen.bottom", &["end"], "Scroll viewport to bottom"),
    ]
}

/// User keybindings in file order: `(id, keys)`, where `None` keys mean "use the default" (TS `undefined`).
pub type UserBindings = Vec<(String, Option<Vec<String>>)>;

fn dedup(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

/// `KeybindingsManager`: resolves user overrides over definitions and matches input against bindings.
///
/// The key-matching state stands in for TypeScript's global one: every clone of a manager shares it, so the
/// keyboard protocol's negotiation ([`KeybindingsManager::set_kitty_active`]) reaches every holder at once.
#[derive(Clone)]
pub struct KeybindingsManager {
    definitions: Vec<KeybindingDefinition>,
    user: UserBindings,
    keys_by_id: Vec<(&'static str, Vec<String>)>,
    ctx: Rc<Cell<KeyContext>>,
}

impl KeybindingsManager {
    pub fn new(definitions: Vec<KeybindingDefinition>, user: UserBindings, ctx: KeyContext) -> Self {
        let mut m = KeybindingsManager {
            definitions,
            user,
            keys_by_id: Vec::new(),
            ctx: Rc::new(Cell::new(ctx)),
        };
        m.rebuild();
        m
    }

    fn rebuild(&mut self) {
        self.keys_by_id = self
            .definitions
            .iter()
            .map(|d| {
                let user = self
                    .user
                    .iter()
                    .find(|(id, _)| id == d.id)
                    .and_then(|(_, k)| k.as_ref());
                (d.id, dedup(user.unwrap_or(&d.default_keys)))
            })
            .collect();
    }

    pub fn set_kitty_active(&self, active: bool) {
        self.ctx.set(KeyContext {
            kitty_active: active,
            ..self.ctx.get()
        });
    }

    /// The key-matching state this manager matches with.
    pub fn ctx(&self) -> KeyContext {
        self.ctx.get()
    }

    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.get_keys(keybinding)
            .iter()
            .any(|k| matches_key(data, k, self.ctx()))
    }

    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id
            .iter()
            .find(|(id, _)| *id == keybinding)
            .map(|(_, k)| k.clone())
            .unwrap_or_default()
    }

    pub fn set_user_bindings(&mut self, user: UserBindings) {
        self.user = user;
        self.rebuild();
    }
}
