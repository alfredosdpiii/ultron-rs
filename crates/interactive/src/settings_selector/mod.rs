//! Pi's `/settings` (`SettingsSelectorComponent`, modes/interactive/components/settings-selector.ts, on the TUI's
//! `SettingsList` with search and submenus, settings-submenu.ts, models-settings.ts and the native TUI's Models
//! section, experimental/client-tui-models-settings.ts). It reads a snapshot of the settings ([`SettingsInput`])
//! and returns [`SettingsEvent`]s; the client saves each change and applies the live ones.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Value, json};
use theme::{Fg, Theme};
use tui::Component;
use tui::Input;
use tui::KeybindingsManager;
use tui::fuzzy_filter;
use tui::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use tui::{SelectEvent, SelectItem, SelectList};
use tui::{truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::bold;
use crate::lane::ModelInfo;

mod items;
mod submenus;
mod widgets;

use submenus::{automatic_themes, set_path};
use widgets::{List, ListAction, SelectOutcome, SelectSubmenu};

use config::providers::{CLAUDE_CODE, CLAUDE_CODE_DEFAULT_MODEL, CLAUDE_CODE_DEFAULT_REF};
const CLEAR_OVERRIDE: &str = "__clear__";
const GROUP_PREFIX: &str = "\u{0}group:";
/// The Models row whose picker toggles models in a list (`rlm.raceModels`) instead of choosing one.
const RACE_MODELS: &str = "rlm.raceModels";
const AUTOMATIC_THEME: &str = "/";
const HTTP_IDLE: [(&str, u64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

fn thinking_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        _ => "Maximum reasoning",
    }
}

/// `formatHttpIdleTimeoutMs`.
pub fn format_http_idle(ms: u64) -> String {
    HTTP_IDLE
        .iter()
        .find(|(_, v)| *v == ms)
        .map_or_else(|| format!("{} sec", ms as f64 / 1000.0), |(l, _)| (*l).to_string())
}

/// The token caps' choices (`rootBudget.maxTotalTokens`, `rlm.mapTokens`); off, the default, removes the key.
const REQUEST_TOKEN_CAPS: [&str; 5] = ["off", "10M", "25M", "50M", "100M"];
const MAP_TOKEN_CAPS: [&str; 5] = ["off", "500k", "1M", "2M", "10M"];

/// A saved token cap as `/settings` shows it: a positive integer, shortened; anything else is off.
fn format_token_cap(value: &Value) -> String {
    match value.as_u64() {
        Some(n) if n >= 1_000_000 && n.is_multiple_of(1_000_000) => format!("{}M", n / 1_000_000),
        Some(n) if n >= 1_000 && n.is_multiple_of(1_000) => format!("{}k", n / 1_000),
        Some(n) if n >= 1 => n.to_string(),
        _ => "off".into(),
    }
}

/// What a token-cap choice saves: off removes the key, else the count.
fn token_cap_value(label: &str) -> Value {
    let (digits, scale) = match (label.strip_suffix('M'), label.strip_suffix('k')) {
        (Some(digits), _) => (digits, 1_000_000),
        (_, Some(digits)) => (digits, 1_000),
        _ => (label, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .map_or(Value::Null, |n| Value::from(n * scale))
}

/// `judge.provider` choices (`judge.provider`), as `/settings` shows them.
const JUDGE_ORDERS: [&str; 5] = ["jev, d1", "d1, jev", "jev", "d1", "none"];
const JUDGE_BUDGETS: [&str; 5] = ["off", "$0.10", "$1", "$5", "$20"];

/// A saved `judge.provider` as a choice: unset is both, Jev first.
fn judge_order(value: &Value) -> String {
    match value.as_array() {
        None => JUDGE_ORDERS[0].into(),
        Some(names) if names.is_empty() => "none".into(),
        Some(names) => names.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
    }
}

/// What a `judge.provider` choice saves.
fn judge_order_value(label: &str) -> Value {
    let names: Vec<&str> = label.split(", ").filter(|n| *n != "none").collect();
    Value::from(names)
}

/// `memory.backend` choices: detect (unset), then the backends.
const MEMORY_BACKENDS: [&str; 3] = [
    "detect",
    config::keys::MEMORY_BACKENDS[0],
    config::keys::MEMORY_BACKENDS[1],
];

/// A saved `judge.budgetUsd` as a choice.
fn budget_label(value: &Value) -> String {
    match value.as_f64().filter(|n| *n > 0.0) {
        Some(usd) if usd < 1.0 => format!("${usd:.2}"),
        Some(usd) => format!("${usd}"),
        None => "off".into(),
    }
}

/// What a `judge.budgetUsd` choice saves: off removes the key.
fn budget_value(label: &str) -> Value {
    label
        .strip_prefix('$')
        .and_then(|n| n.parse::<f64>().ok())
        .map_or(Value::Null, Value::from)
}

/// A token-cap row; a saved count that is not one of the choices is offered after off.
fn token_cap_item(id: &str, label: &str, description: &str, saved: &Value, choices: &[&str]) -> Item {
    let current = format_token_cap(saved);
    let mut values = choices.to_vec();
    if !values.contains(&current.as_str()) {
        values.insert(1, &current);
    }
    item(id, label, description, current.clone(), &values)
}

/// What `/settings` reads.
pub struct SettingsInput {
    /// The merged settings document.
    pub settings: Value,
    /// Model settings an environment variable overrides: key -> (variable, value).
    pub env_overrides: BTreeMap<String, (String, String)>,
    pub models: Vec<ModelInfo>,
    pub session: Option<ModelInfo>,
    pub themes: Vec<String>,
    /// `light` or `dark`.
    pub terminal_theme: String,
    pub supports_images: bool,
    /// Formatted `app.message.followUp` and `app.thinking.cycle` keys (`keyDisplayText`).
    pub follow_up_key: String,
    pub cycle_thinking_key: String,
    /// Rows of Rust-only settings (`persona`); off in the TypeScript-parity build (`ULTRON_TUI_PARITY=1`), which
    /// shows exactly TypeScript's rows.
    pub rust_rows: bool,
}

impl SettingsInput {
    fn get(&self, key: &str) -> &Value {
        let mut v = &self.settings;
        for part in key.split('.') {
            v = &v[part];
        }
        v
    }
    fn bool(&self, key: &str, fallback: bool) -> bool {
        self.get(key).as_bool().unwrap_or(fallback)
    }
    fn num(&self, key: &str, fallback: f64) -> f64 {
        self.get(key).as_f64().unwrap_or(fallback)
    }
    fn str(&self, key: &str, fallback: &str) -> String {
        self.get(key).as_str().unwrap_or(fallback).to_string()
    }
    fn text(&self, key: &str) -> Option<String> {
        self.get(key).as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }
    /// `rlm.raceModels`: the picked `provider/model` refs, in order.
    fn race_models(&self) -> Vec<String> {
        self.get(RACE_MODELS)
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingsEvent {
    /// Save `key` (a settings path, or `modelThinkingLevel` with `{provider, modelId, level}`).
    Change(String, Value),
    /// The Models section's session row: switch the session's model and save it as the default.
    SessionModel(String),
    /// A Models row changed (`label`, value, whether it applies to `ultron claude`).
    ModelSetting {
        key: String,
        label: String,
        value: String,
    },
    Cancelled,
    /// `onThemePreview`: show this theme setting (a name or `light/dark`) while the user browses themes.
    Preview(String),
}

fn str_bool(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

#[derive(Clone, PartialEq)]
enum SubKind {
    Warnings,
    Theme,
    ModelThinking,
    Models,
    ModelPick(String),
    LightTheme,
    DarkTheme,
}

#[derive(Clone)]
struct Item {
    id: String,
    label: String,
    description: Option<String>,
    value: String,
    values: Vec<String>,
    sub: Option<SubKind>,
}

fn item(id: &str, label: &str, description: &str, value: String, values: &[&str]) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        value,
        values: values.iter().map(|v| (*v).to_string()).collect(),
        sub: None,
    }
}

/// The open submenu.
enum Sub {
    Select {
        kind: SubKind,
        menu: SelectSubmenu,
    },
    List {
        kind: SubKind,
        list: List,
        header: Vec<String>,
    },
}

pub struct SettingsSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    input: SettingsInput,
    list: List,
    /// Submenus, innermost last.
    stack: Vec<Sub>,
    warnings_extra: bool,
    model_thinking: BTreeMap<String, String>,
    /// The per-model thinking step: the chosen model.
    thinking_model: Option<String>,
    /// The theme submenu's automatic pair.
    light: String,
    dark: String,
}

fn model_ref(m: &ModelInfo) -> String {
    format!("{}/{}", m.provider, m.id)
}

impl SettingsSelector {
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, input: SettingsInput) -> Self {
        let model_thinking: BTreeMap<String, String> = input
            .settings
            .as_object()
            .map_or_default(config::keys::model_thinking_levels)
            .into_iter()
            .collect();
        let theme_setting = input.str("theme", "dark");
        let (light, dark) = automatic_themes(&theme_setting, &input.themes);
        let mut selector = SettingsSelector {
            list: List::new(Rc::clone(&theme), keys.clone(), Vec::new(), 10, true),
            theme,
            keys,
            warnings_extra: input.get("warnings")["anthropicExtraUsage"].as_bool().unwrap_or(true),
            input,
            stack: Vec::new(),
            model_thinking,
            thinking_model: None,
            light,
            dark,
        };
        let items = selector.items();
        selector.list.items = items;
        selector.list.filtered = (0..selector.list.items.len()).collect();
        selector
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Vec<SettingsEvent>)> {
        if event.y < 1 {
            return None;
        }
        let width = event.width;
        let header_rows = |header: &[String]| -> i64 {
            header
                .iter()
                .map(|h| {
                    if h.is_empty() {
                        1
                    } else {
                        wrap_text_with_ansi(h, width).len() as i64
                    }
                })
                .sum()
        };
        let mut y = event.y - 1;
        let depth = self.stack.len();
        for sub in self.stack.iter().take(depth.saturating_sub(1)) {
            if let Sub::List { header, .. } = sub {
                y -= header_rows(header);
            }
        }
        let focused = |result: MouseResult| MouseResult {
            focus: Some(true),
            ..result
        };
        let Some(sub) = self.stack.pop() else {
            let local = MouseEvent { y, ..*event };
            let (result, action) = self.list.handle_mouse(&local)?;
            return Some((result, action.and_then(|a| self.main_action(a)).into_iter().collect()));
        };
        match sub {
            Sub::List { kind, mut list, header } => {
                let local = MouseEvent {
                    y: y - header_rows(&header),
                    ..*event
                };
                match list.handle_mouse(&local) {
                    None => {
                        self.stack.push(Sub::List { kind, list, header });
                        None
                    }
                    Some((result, action)) => {
                        let event = self.list_sub(kind, list, header, action);
                        Some((focused(result), event.into_iter().collect()))
                    }
                }
            }
            Sub::Select { kind, mut menu } => {
                let local = MouseEvent { y, ..*event };
                let Some((result, outcomes)) = menu.handle_mouse(&local) else {
                    self.stack.push(Sub::Select { kind, menu });
                    return None;
                };
                let mut events = Vec::new();
                let mut open = Some((kind, menu));
                for outcome in outcomes {
                    let Some((kind, menu)) = open.take() else { break };
                    events.extend(self.select_sub(kind.clone(), menu, Some(outcome)));
                    if let Some(Sub::Select { kind: top, .. }) = self.stack.last()
                        && *top == kind
                        && let Some(Sub::Select { kind, menu }) = self.stack.pop()
                    {
                        open = Some((kind, menu));
                    }
                }
                if let Some((kind, menu)) = open {
                    self.stack.push(Sub::Select { kind, menu });
                }
                Some((focused(result), events))
            }
        }
    }

    pub fn handle_input(&mut self, data: &str) -> Option<SettingsEvent> {
        let Some(sub) = self.stack.pop() else {
            let action = self.list.handle_input(data)?;
            return self.main_action(action);
        };
        match sub {
            Sub::List { kind, mut list, header } => {
                let action = list.handle_input(data);
                self.list_sub(kind, list, header, action)
            }
            Sub::Select { kind, mut menu } => {
                let outcome = menu.handle_input(data);
                self.select_sub(kind, menu, outcome)
            }
        }
    }

    /// What the main list's `action` does.
    fn main_action(&mut self, action: ListAction) -> Option<SettingsEvent> {
        match action {
            ListAction::Cancel => Some(SettingsEvent::Cancelled),
            ListAction::Open(index) => {
                if let Some(kind) = self.list.items[index].sub.clone() {
                    self.open(kind);
                }
                None
            }
            ListAction::Changed(id, value) => Some(self.main_change(&id, &value)),
        }
    }

    /// What `action` of the open list submenu (popped off the stack) does; the submenu goes back when it stays.
    fn list_sub(
        &mut self,
        kind: SubKind,
        list: List,
        header: Vec<String>,
        action: Option<ListAction>,
    ) -> Option<SettingsEvent> {
        match (action, &kind) {
            (Some(ListAction::Cancel), SubKind::Models) => {
                let summary = self.models_summary();
                self.list.update("models", &summary);
                None
            }
            (Some(ListAction::Cancel), SubKind::Theme) => Some(SettingsEvent::Preview(self.input.str("theme", "dark"))),
            (Some(ListAction::Cancel), _) => None,
            (Some(ListAction::Open(index)), _) => {
                let next = list.items[index].sub.clone();
                self.stack.push(Sub::List { kind, list, header });
                if let Some(next) = next {
                    self.open(next);
                }
                None
            }
            (Some(ListAction::Changed(_, value)), SubKind::Warnings) => {
                self.warnings_extra = value == "true";
                self.stack.push(Sub::List { kind, list, header });
                Some(SettingsEvent::Change(
                    "warnings".into(),
                    json!({"anthropicExtraUsage": self.warnings_extra}),
                ))
            }
            (Some(ListAction::Changed(id, _)), SubKind::Theme) => {
                if id == "apply" {
                    return Some(self.set_theme(format!("{}/{}", self.light, self.dark)));
                }
                self.stack.push(self.theme_single());
                Some(SettingsEvent::Preview(self.active_automatic_theme()))
            }
            (_, _) => {
                self.stack.push(Sub::List { kind, list, header });
                None
            }
        }
    }

    /// What `outcome` of the open select submenu (popped off the stack) does; the submenu goes back when it stays.
    fn select_sub(
        &mut self,
        kind: SubKind,
        menu: SelectSubmenu,
        outcome: Option<SelectOutcome>,
    ) -> Option<SettingsEvent> {
        match outcome {
            None => {
                self.stack.push(Sub::Select { kind, menu });
                None
            }
            Some(SelectOutcome::Highlight(value)) => {
                let preview = match kind {
                    SubKind::Theme if value == AUTOMATIC_THEME => Some(self.automatic_setting()),
                    SubKind::Theme | SubKind::LightTheme | SubKind::DarkTheme => Some(value),
                    _ => None,
                };
                self.stack.push(Sub::Select { kind, menu });
                preview.map(SettingsEvent::Preview)
            }
            Some(SelectOutcome::Back) => {
                match kind {
                    SubKind::Theme => return Some(SettingsEvent::Preview(self.input.str("theme", "dark"))),
                    SubKind::LightTheme | SubKind::DarkTheme => {
                        return Some(SettingsEvent::Preview(self.automatic_setting()));
                    }
                    _ => {}
                }
                if matches!(kind, SubKind::ModelThinking) && self.thinking_model.take().is_some() {
                    self.stack.push(self.thinking_step());
                }
                if matches!(kind, SubKind::ModelThinking) {
                    self.list.update("model-thinking", &self.thinking_summary());
                }
                None
            }
            Some(SelectOutcome::Picked(value)) => self.picked(kind, value),
        }
    }

    fn picked(&mut self, kind: SubKind, value: String) -> Option<SettingsEvent> {
        match kind {
            SubKind::Theme => {
                if value == AUTOMATIC_THEME {
                    self.stack.push(self.theme_automatic());
                    return Some(SettingsEvent::Preview(self.automatic_setting()));
                }
                Some(self.set_theme(value))
            }
            SubKind::LightTheme | SubKind::DarkTheme => {
                if matches!(kind, SubKind::LightTheme) {
                    self.light = value.clone();
                } else {
                    self.dark = value.clone();
                }
                if let Some(Sub::List { list, .. }) = self.stack.last_mut() {
                    list.update(
                        if matches!(kind, SubKind::LightTheme) {
                            "light-theme"
                        } else {
                            "dark-theme"
                        },
                        &value,
                    );
                }
                Some(SettingsEvent::Preview(self.automatic_setting()))
            }
            SubKind::ModelThinking => match self.thinking_model.take() {
                None => {
                    if value != "__none__" {
                        self.thinking_model = Some(value);
                    }
                    self.stack.push(self.thinking_step());
                    None
                }
                Some(key) => {
                    let (provider, id) = key.split_once('/').unwrap_or(("", key.as_str()));
                    let level = if value == CLEAR_OVERRIDE {
                        self.model_thinking.remove(&key);
                        Value::Null
                    } else {
                        self.model_thinking.insert(key.clone(), value.clone());
                        Value::String(value)
                    };
                    let event = SettingsEvent::Change(
                        "modelThinkingLevel".into(),
                        json!({"provider": provider, "modelId": id, "level": level}),
                    );
                    self.stack.push(self.thinking_step());
                    Some(event)
                }
            },
            SubKind::ModelPick(id) => {
                if value.starts_with(GROUP_PREFIX) {
                    if let Some(row) = self.model_rows().into_iter().find(|r| r.id == id) {
                        self.stack.push(Sub::Select {
                            menu: SelectSubmenu::new(
                                Rc::clone(&self.theme),
                                self.keys.clone(),
                                (&row.label, &row.description),
                                self.picker_items(&row),
                                &row.current,
                                true,
                                (12, 46),
                            ),
                            kind: SubKind::ModelPick(id),
                        });
                    }
                    return None;
                }
                let row = self.model_rows().into_iter().find(|r| r.id == id)?;
                let event = if id == RACE_MODELS {
                    let mut picked = self.input.race_models();
                    if value.is_empty() {
                        picked.clear();
                    } else if let Some(i) = picked.iter().position(|m| *m == value) {
                        picked.remove(i);
                    } else {
                        picked.push(value.clone());
                    }
                    let stored = if picked.is_empty() { Value::Null } else { json!(picked) };
                    set_path(&mut self.input.settings, &id, stored);
                    SettingsEvent::ModelSetting {
                        key: id.clone(),
                        label: row.label,
                        value: picked.join(", "),
                    }
                } else if id == "session" {
                    if let Some(m) = self.input.models.iter().find(|m| model_ref(m) == value) {
                        self.input.session = Some(m.clone());
                    }
                    SettingsEvent::SessionModel(value.clone())
                } else {
                    let stored = if value.is_empty() {
                        Value::Null
                    } else if id == "claudeCode.model" || id == "claudeCode.childModel" {
                        Value::String(value.trim_start_matches("claude-code/").to_string())
                    } else {
                        Value::String(value.clone())
                    };
                    set_path(&mut self.input.settings, &id, stored);
                    SettingsEvent::ModelSetting {
                        key: id.clone(),
                        label: row.label,
                        value: value.clone(),
                    }
                };
                let rows = self.model_rows();
                if let Some(Sub::List { list, .. }) = self.stack.last_mut() {
                    for row in rows {
                        if !row.heading {
                            let shown = match &row.locked {
                                None => row.display.clone(),
                                Some(_) => format!("{} · locked", row.display),
                            };
                            list.update(&row.id, &shown);
                        }
                    }
                }
                if id == RACE_MODELS
                    && let Some(row) = self.model_rows().into_iter().find(|r| r.id == id)
                {
                    self.stack.push(Sub::Select {
                        menu: SelectSubmenu::new(
                            Rc::clone(&self.theme),
                            self.keys.clone(),
                            (&row.label, &row.description),
                            self.picker_items(&row),
                            &value,
                            true,
                            (12, 46),
                        ),
                        kind: SubKind::ModelPick(id),
                    });
                }
                Some(event)
            }
            SubKind::Warnings | SubKind::Models => None,
        }
    }

    /// `getAutomaticThemeSetting`.
    pub(super) fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light, self.dark)
    }

    /// `getActiveAutomaticTheme`: the side of the pair the terminal's appearance selects.
    pub(super) fn active_automatic_theme(&self) -> String {
        if self.input.terminal_theme == "light" {
            self.light.clone()
        } else {
            self.dark.clone()
        }
    }

    pub(super) fn set_theme(&mut self, theme: String) -> SettingsEvent {
        self.stack.clear();
        self.list.update("theme", &theme);
        set_path(&mut self.input.settings, "theme", Value::String(theme.clone()));
        SettingsEvent::Change("theme".into(), Value::String(theme))
    }
}

impl Component for SettingsSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = t.fg(Fg::Border, &"─".repeat(width.max(1)));
        let mut lines = vec![border.clone()];
        let depth = self.stack.len();
        for sub in self.stack.iter().take(depth.saturating_sub(1)) {
            if let Sub::List { header, .. } = sub {
                for h in header {
                    if h.is_empty() {
                        lines.push(String::new());
                    } else {
                        lines.extend(wrap_text_with_ansi(h, width));
                    }
                }
            }
        }
        match self.stack.last_mut() {
            None => lines.extend(self.list.render(width)),
            Some(Sub::Select { menu, .. }) => lines.extend(menu.render(width)),
            Some(Sub::List { list, header, .. }) => {
                for h in header.iter() {
                    if h.is_empty() {
                        lines.push(String::new());
                    } else {
                        lines.extend(wrap_text_with_ansi(h, width));
                    }
                }
                lines.extend(list.render(width));
            }
        }
        lines.push(border);
        lines
    }
}

impl SettingsSelector {
    /// The input value of the search line (tests).
    pub fn query(&self) -> String {
        self.list.search.as_ref().map_or_default(|s| s.value().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_http_idle_values_off_the_list_print_as_seconds() {
        assert_eq!(format_http_idle(45_000), "45 sec");
        assert_eq!(format_http_idle(1_500), "1.5 sec");
    }

    #[test]
    fn settings_automatic_themes_split_or_repeat() {
        let themes = vec!["dark".to_string(), "light".to_string()];
        assert_eq!(automatic_themes("light/dark", &themes), ("light".into(), "dark".into()));
        assert_eq!(automatic_themes("light", &themes), ("light".into(), "light".into()));
        assert_eq!(automatic_themes("nope", &themes), ("dark".into(), "dark".into()));
    }

    fn selector(settings: Value) -> SettingsSelector {
        let theme = Rc::new(
            theme::create_theme(
                &theme::builtin_theme_json("dark").unwrap(),
                theme::ColorMode::Truecolor,
                None,
            )
            .unwrap(),
        );
        let keys = KeybindingsManager::new(
            crate::keybindings::native_app_keybindings(crate::keybindings::Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        );
        let model = |provider: &str, id: &str| ModelInfo {
            provider: provider.into(),
            id: id.into(),
            ..Default::default()
        };
        let input = SettingsInput {
            settings,
            env_overrides: BTreeMap::new(),
            models: vec![
                model("anthropic", "claude-haiku-4-5"),
                model(CLAUDE_CODE, "claude-opus-5-5"),
            ],
            session: Some(model(CLAUDE_CODE, "claude-opus-5-5")),
            themes: vec!["dark".into(), "light".into()],
            terminal_theme: "dark".into(),
            supports_images: false,
            follow_up_key: String::new(),
            cycle_thinking_key: String::new(),
            rust_rows: true,
        };
        SettingsSelector::new(theme, keys, input)
    }

    /// claude-code models run their tool lanes on the Claude subscription: the Session, sub-agent and `ultron claude`
    /// Ultron sub-agent rows offer them, and the Ultron sub-agent row's unset value is the root's subscription model.
    #[test]
    fn settings_tool_rows_offer_claude_code_models() {
        let rows = selector(json!({"defaultProvider": "anthropic", "defaultModel": "claude-haiku-4-5"})).model_rows();
        for id in ["session", "rlm.childModel", "claudeCode.ultronChildModel"] {
            let row = rows.iter().find(|r| r.id == id).unwrap();
            assert!(
                row.choices.iter().any(|c| c.0 == "claude-code/claude-opus-5-5"),
                "{id}: {:?}",
                row.choices
            );
        }
        let row = rows.iter().find(|r| r.id == "claudeCode.ultronChildModel").unwrap();
        assert_eq!(
            (row.display.as_str(), row.choices[0].1.as_str()),
            ("root's subscription model", "Root's subscription model")
        );
    }

    /// A durable session (its client marks `durable`): the Models list has the durable default model's row, and the
    /// Session model row names `durable.model` as the saved default. Plain `ultron` has neither.
    #[test]
    fn settings_durable_sessions_list_durable_model_as_their_saved_default() {
        let global = json!({"defaultProvider": "anthropic", "defaultModel": "claude-haiku-4-5"});
        let rows = selector(global.clone()).model_rows();
        assert!(rows.iter().all(|r| r.id != "durable.model"));
        assert!(
            rows[0]
                .description
                .ends_with("Saved default: anthropic/claude-haiku-4-5."),
            "{}",
            rows[0].description
        );
        let mut durable = global.clone();
        durable["durable"] = json!({});
        let rows = selector(durable).model_rows();
        let row = &rows[1];
        assert_eq!(
            (
                row.id.as_str(),
                row.label.as_str(),
                row.display.as_str(),
                row.current.as_str()
            ),
            (
                "durable.model",
                "Durable default model",
                "claude-code/claude-opus-5-5 (default)",
                ""
            )
        );
        assert_eq!(row.choices[0].1, "Default (claude-code/claude-opus-5-5)");
        assert!(row.choices.iter().any(|c| c.0 == "claude-code/claude-opus-5-5"));
        assert!(
            rows[0]
                .description
                .ends_with("Saved default: claude-code/claude-opus-5-5 (durable.model)."),
            "{}",
            rows[0].description
        );
        let mut chosen = global;
        chosen["durable"] = json!({"model": "anthropic/claude-haiku-4-5"});
        let rows = selector(chosen).model_rows();
        assert_eq!(
            (rows[1].display.as_str(), rows[1].current.as_str()),
            ("anthropic/claude-haiku-4-5 (setting)", "anthropic/claude-haiku-4-5")
        );
        assert!(
            rows[0]
                .description
                .ends_with("Saved default: anthropic/claude-haiku-4-5 (durable.model).")
        );
    }

    /// Unset queue modes show `all`, the session's default (client-tui-settings.ts `str("steeringMode", "all")`).
    #[test]
    fn settings_queue_modes_show_the_session_default_unset() {
        let s = selector(json!({}));
        let value = |id: &str| s.list.items.iter().find(|it| it.id == id).unwrap().value.clone();
        assert_eq!(value("steering-mode"), config::keys::QUEUE_DEFAULT);
        assert_eq!(value("follow-up-mode"), config::keys::QUEUE_DEFAULT);
        let s = selector(json!({"steeringMode": "one-at-a-time"}));
        assert_eq!(
            s.list.items.iter().find(|it| it.id == "steering-mode").unwrap().value,
            "one-at-a-time"
        );
    }

    /// The token caps are off by default; a choice saves its count and off removes the key. A hand-set count that is
    /// not a choice is shown and offered.
    #[test]
    fn settings_token_caps_default_off() {
        let row = |s: &SettingsSelector, id: &str| {
            let it = s.list.items.iter().find(|it| it.id == id).unwrap();
            (it.value.clone(), it.values.clone())
        };
        let mut s = selector(json!({}));
        assert_eq!(row(&s, "request-token-cap").0, "off");
        assert_eq!(row(&s, "map-token-cap").0, "off");
        assert_eq!(
            s.main_change("request-token-cap", "25M"),
            SettingsEvent::Change("rootBudget.maxTotalTokens".into(), json!(25_000_000))
        );
        assert_eq!(
            s.main_change("map-token-cap", "500k"),
            SettingsEvent::Change("rlm.mapTokens".into(), json!(500_000))
        );
        assert_eq!(
            s.main_change("map-token-cap", "off"),
            SettingsEvent::Change("rlm.mapTokens".into(), Value::Null)
        );
        let s = selector(json!({"rootBudget": {"maxTotalTokens": 3_000_000}, "rlm": {"mapTokens": "off"}}));
        let (value, values) = row(&s, "request-token-cap");
        assert_eq!(
            (value.as_str(), &values[..2]),
            ("3M", &["off".to_string(), "3M".to_string()][..])
        );
        assert_eq!(row(&s, "map-token-cap").0, "off");
    }

    /// `rlm.resultFooter`: off unless set; the row saves a boolean.
    #[test]
    fn settings_result_footer_row() {
        let value = |s: &SettingsSelector| {
            s.list
                .items
                .iter()
                .find(|it| it.id == "result-footer")
                .unwrap()
                .value
                .clone()
        };
        let mut s = selector(json!({}));
        assert_eq!(value(&s), "false");
        assert_eq!(
            s.main_change("result-footer", "true"),
            SettingsEvent::Change("rlm.resultFooter".into(), json!(true))
        );
        assert_eq!(value(&selector(json!({"rlm": {"resultFooter": true}}))), "true");
    }

    /// `memory.backend`: unset shows detect; a backend saves its name, detect removes the key.
    #[test]
    fn settings_memory_backend_row() {
        let value = |s: &SettingsSelector| {
            s.list
                .items
                .iter()
                .find(|it| it.id == "memory-backend")
                .unwrap()
                .value
                .clone()
        };
        let mut s = selector(json!({}));
        assert_eq!(value(&s), "detect");
        assert_eq!(
            s.main_change("memory-backend", "memorantado"),
            SettingsEvent::Change("memory.backend".into(), json!("memorantado"))
        );
        assert_eq!(
            s.main_change("memory-backend", "detect"),
            SettingsEvent::Change("memory.backend".into(), Value::Null)
        );
        assert_eq!(
            value(&selector(json!({"memory": {"backend": "hindsight"}}))),
            "hindsight"
        );
    }

    /// Race models: picking toggles a model in `rlm.raceModels` and keeps the picker open with every picked model
    /// marked; Clear empties the list (removes the key); the row shows the list or "none (skills choose)".
    #[test]
    fn settings_race_models_toggle_in_the_picker() {
        let display = |s: &SettingsSelector| {
            s.model_rows()
                .into_iter()
                .find(|r| r.id == RACE_MODELS)
                .unwrap()
                .display
        };
        let marked = |s: &SettingsSelector| {
            let row = s.model_rows().into_iter().find(|r| r.id == RACE_MODELS).unwrap();
            s.picker_items(&row)
                .into_iter()
                .filter(|i| i.label.starts_with("✓ "))
                .map(|i| i.value)
                .collect::<Vec<_>>()
        };
        let pick = |s: &mut SettingsSelector, value: &str| {
            let event = s.picked(SubKind::ModelPick(RACE_MODELS.into()), value.into());
            assert!(
                matches!(s.stack.last(), Some(Sub::Select { kind: SubKind::ModelPick(id), .. }) if id == RACE_MODELS),
                "the picker stays open"
            );
            event
        };
        let haiku = "anthropic/claude-haiku-4-5";
        let opus = "claude-code/claude-opus-5-5";
        let mut s = selector(json!({}));
        assert_eq!(display(&s), "none (skills choose)");
        let event = pick(&mut s, haiku);
        assert_eq!(
            event,
            Some(SettingsEvent::ModelSetting {
                key: RACE_MODELS.into(),
                label: "Race models".into(),
                value: haiku.into()
            })
        );
        pick(&mut s, opus);
        assert_eq!(s.input.get(RACE_MODELS), &json!([haiku, opus]));
        assert_eq!(display(&s), format!("{haiku}, {opus}"));
        assert_eq!(marked(&s), [haiku, opus]);
        let event = pick(&mut s, haiku);
        assert!(matches!(event, Some(SettingsEvent::ModelSetting { value, .. }) if value == opus));
        assert_eq!(s.input.get(RACE_MODELS), &json!([opus]));
        assert_eq!(marked(&s), [opus]);
        let event = pick(&mut s, "");
        assert!(matches!(event, Some(SettingsEvent::ModelSetting { value, .. }) if value.is_empty()));
        assert_eq!(s.input.get(RACE_MODELS), &Value::Null);
        assert_eq!(display(&s), "none (skills choose)");
        assert!(marked(&s).is_empty());
    }
    #[test]
    fn settings_observer_model_selects_and_clears_without_switching_session() {
        let mut s = selector(json!({}));
        let row = s.model_rows().into_iter().find(|r| r.id == "observer.model").unwrap();
        assert_eq!(row.label, "Observer model");
        assert_eq!(row.choices[0].1, "Same as session model");
        assert!(row.choices.iter().any(|c| c.0 == "anthropic/claude-haiku-4-5"));
        let before = s.input.session.as_ref().map(model_ref);
        assert_eq!(
            s.picked(
                SubKind::ModelPick("observer.model".into()),
                "anthropic/claude-haiku-4-5".into()
            ),
            Some(SettingsEvent::ModelSetting {
                key: "observer.model".into(),
                label: "Observer model".into(),
                value: "anthropic/claude-haiku-4-5".into()
            })
        );
        assert_eq!(s.input.get("observer.model"), &json!("anthropic/claude-haiku-4-5"));
        assert_eq!(s.input.session.as_ref().map(model_ref), before);
        s.picked(SubKind::ModelPick("observer.model".into()), String::new());
        assert_eq!(s.input.get("observer.model"), &Value::Null);
    }
}
