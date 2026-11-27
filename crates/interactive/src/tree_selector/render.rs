use std::rc::Rc;

use theme::{Bg, Fg, Theme};
use tui::Component;
use tui::Input;
use tui::KeybindingsManager;
use tui::{slice_with_width, truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::format_key_text;

use super::format::{display_text, format_label_timestamp};
use super::{Flat, TreeList, id_of};

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

impl TreeList {
    fn connector_char(&self, f: &Flat, id: &str, pos: usize, folded: bool) -> char {
        match pos {
            0 if f.is_last => '└',
            0 => '├',
            1 if folded => '⊞',
            1 if self.is_foldable(id) => '⊟',
            1 => '─',
            _ => ' ',
        }
    }

    fn row_prefix(&self, f: &Flat, id: &str, connector: bool, folded: bool) -> String {
        let display_indent = if self.multiple_roots {
            f.indent.saturating_sub(1)
        } else {
            f.indent
        };
        let connector_position = if connector { display_indent as isize - 1 } else { -1 };
        let mut prefix = String::new();
        for c in 0..display_indent * 3 {
            let level = c / 3;
            let pos = c % 3;
            if let Some(g) = f.gutters.iter().find(|g| g.position == level) {
                prefix.push(if pos == 0 && g.show { '│' } else { ' ' });
            } else if connector && level as isize == connector_position {
                prefix.push(self.connector_char(f, id, pos, folded));
            } else {
                prefix.push(' ');
            }
        }
        prefix
    }

    fn render_row(&self, index: usize) -> (String, String, usize, usize, bool) {
        let t = &self.theme;
        let f = &self.filtered[index];
        let node = &self.nodes[f.node];
        let id = id_of(&node.entry);
        let selected = index == self.selected;
        let cursor = if selected {
            t.fg(Fg::Accent, "› ")
        } else {
            "  ".to_string()
        };
        let connector = f.show_connector && !f.virtual_root_child;
        let folded = self.folded.contains(id);
        let prefix = self.row_prefix(f, id, connector, folded);
        let fold_marker = if folded && !connector {
            t.fg(Fg::Accent, "⊞ ")
        } else {
            String::new()
        };
        let path_marker = if self.active_path.contains(id) {
            t.fg(Fg::Accent, "• ")
        } else {
            String::new()
        };
        let label = node
            .label
            .as_ref()
            .map_or_default(|l| t.fg(Fg::Warning, &format!("[{l}] ")));
        let label_time = match (&node.label, &node.label_timestamp) {
            (Some(_), Some(ts)) if self.show_label_timestamps => {
                t.fg(Fg::Muted, &format!("{} ", format_label_timestamp(ts)))
            }
            _ => String::new(),
        };
        let text = display_text(t, &self.tool_calls, &self.home, node, selected);
        let prefix_part = t.fg(Fg::Dim, &prefix) + &fold_marker + &path_marker;
        let anchor = visible_width(&prefix_part);
        let mut gutter = cursor;
        let mut body = prefix_part + &label + &label_time + &text;
        if selected {
            gutter = t.bg(Bg::SelectedBg, &gutter);
            body = t.bg(Bg::SelectedBg, &body);
        }
        let body_width = visible_width(&body);
        (gutter, body, anchor, body_width, selected)
    }

    pub(super) fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        if self.filtered.is_empty() {
            return vec![
                truncate_to_width(&t.fg(Fg::Muted, "  No entries found"), width, "...", false),
                truncate_to_width(
                    &t.fg(Fg::Muted, &format!("  (0/0){}", self.status_labels())),
                    width,
                    "...",
                    false,
                ),
            ];
        }
        let n = self.filtered.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        let end = (start + self.max_visible).min(self.filtered.len());
        let rows: Vec<_> = (start..end).map(|i| self.render_row(i)).collect();
        let mut lines = horizontal_viewport(&rows, width);
        lines.push(truncate_to_width(
            &t.fg(
                Fg::Muted,
                &format!(
                    "  ({}/{}){}",
                    self.selected + 1,
                    self.filtered.len(),
                    self.status_labels()
                ),
            ),
            width,
            "...",
            false,
        ));
        lines
    }
}

/// `renderHorizontalViewport`: the gutter stays, the bodies pan when the selected row's anchor is too far right.
fn horizontal_viewport(rows: &[(String, String, usize, usize, bool)], width: usize) -> Vec<String> {
    let viewport = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body = rows.iter().map(|r| r.3).max().unwrap_or(0);
    let max_scroll = max_body.saturating_sub(viewport);
    let mut scroll = 0;
    if let Some(selected) = rows.iter().find(|r| r.4)
        && max_scroll > 0
    {
        let min_visible = (viewport / 3).clamp(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH, MAX_VISIBLE_ANCHOR_CONTENT_WIDTH);
        if selected.2 as isize > viewport as isize - min_visible as isize {
            let context = (viewport / 4).clamp(MIN_ANCHOR_CONTEXT_WIDTH, MAX_ANCHOR_CONTEXT_WIDTH);
            scroll = max_scroll.min(selected.2.saturating_sub(context));
        }
    }
    rows.iter()
        .map(|(gutter, body, ..)| {
            let line = if scroll > 0 {
                format!("{gutter}{}\x1b[0m", slice_with_width(body, scroll, viewport, true).0)
            } else {
                format!("{gutter}{body}")
            };
            truncate_to_width(&line, width, "", false)
        })
        .collect()
}

/// `TREE_HELP_ITEMS`.
const HELP: [(&[&str], &str, bool); 8] = [
    (&["tui.select.up", "tui.select.down"], "move", false),
    (&["tui.editor.cursorLeft", "tui.editor.cursorRight"], "page", false),
    (&["app.tree.foldOrUp", "app.tree.unfoldOrDown"], "branch", false),
    (&["app.message.copy"], "copy", false),
    (&["app.tree.editLabel"], "label", false),
    (&["app.tree.toggleLabelTimestamp"], "label time", false),
    (
        &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        "filters",
        true,
    ),
    (
        &["app.tree.filter.cycleForward", "app.tree.filter.cycleBackward"],
        "cycle",
        true,
    ),
];

/// `formatHelpKeys`.
fn help_keys(keys: &KeybindingsManager, ids: &[&str]) -> String {
    let first: Vec<String> = ids.iter().filter_map(|id| keys.get_keys(id).first().cloned()).collect();
    if first.is_empty() {
        return String::new();
    }
    let compact = if first.len() == 1 {
        first[0].clone()
    } else {
        let parts: Vec<(String, String)> = first
            .iter()
            .map(|k| match k.rfind('+') {
                Some(i) => (k[..=i].to_string(), k[i + 1..].to_string()),
                None => (String::new(), k.clone()),
            })
            .collect();
        let prefix = &parts[0].0;
        if !prefix.is_empty() && parts.iter().all(|p| &p.0 == prefix) {
            format!(
                "{prefix}{}",
                parts.iter().map(|p| p.1.as_str()).collect::<Vec<_>>().join("/")
            )
        } else {
            first.join("/")
        }
    };
    let text = format_key_text(&compact, false);
    word_replace(&text)
}

/// The `\bpageUp\b`, `\bup\b`, ... replacements of `formatHelpKeys`.
fn word_replace(text: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(match word.as_str() {
            "pageUp" => "pgup",
            "pageDown" => "pgdn",
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            w => w,
        });
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// The help item of the file-restoring rewind, shown only when some entry has a checkpoint (so a session without
/// checkpoints shows TypeScript's help).
const RESTORE_HELP: (&[&str], &str, bool) = (&["app.tree.restoreFiles"], "restore files", false);

fn format_help_item(keys: &KeybindingsManager, ids: &[&str], label: &str, label_first: bool) -> String {
    let text = help_keys(keys, ids);
    if text.is_empty() {
        label.to_string()
    } else if label_first {
        format!("{label} {text}")
    } else {
        format!("{text} {label}")
    }
}

/// `TreeHelp`.
pub(super) fn help_lines(theme: &Theme, keys: &KeybindingsManager, width: usize, checkpoints: bool) -> Vec<String> {
    let mut help = HELP.to_vec();
    if checkpoints {
        help.insert(6, RESTORE_HELP);
    }
    let items: Vec<String> = help
        .iter()
        .map(|(ids, label, label_first)| format_help_item(keys, ids, label, *label_first))
        .collect();
    let available = width.max(1);
    let (indent, separator) = ("  ", " · ");
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in items {
        let indented = format!("{indent}{item}");
        let fits_indented = visible_width(&indented) <= available;
        let fresh = if fits_indented { indented } else { item.clone() };
        let candidate = if current.is_empty() {
            fresh.clone()
        } else {
            format!("{current}{separator}{item}")
        };
        if current.is_empty() || visible_width(&candidate) <= available {
            current = candidate;
            continue;
        }
        lines.extend(wrap_text_with_ansi(current.trim_end(), available));
        current = fresh;
    }
    if !current.is_empty() {
        lines.extend(wrap_text_with_ansi(current.trim_end(), available));
    }
    lines.into_iter().map(|l| theme.fg(Fg::Muted, &l)).collect()
}

pub(super) fn render_label_editor(
    t: &Theme,
    keys: &KeybindingsManager,
    input: &mut Input,
    width: usize,
) -> Vec<String> {
    let indent = "  ";
    let mut lines = vec![truncate_to_width(
        &format!("{indent}{}", t.fg(Fg::Muted, "Label (empty to remove):")),
        width,
        "...",
        false,
    )];
    for l in input.render(width.saturating_sub(indent.len())) {
        lines.push(truncate_to_width(&format!("{indent}{l}"), width, "...", false));
    }
    let hint = |id: &str, d: &str| crate::components::key_hint(t, &crate::components::key_text(keys, id), d);
    lines.push(truncate_to_width(
        &format!(
            "{indent}{}  {}",
            hint("tui.select.confirm", "save"),
            hint("tui.select.cancel", "cancel")
        ),
        width,
        "...",
        false,
    ));
    lines
}
