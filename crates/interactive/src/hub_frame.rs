//! The rounded box the model hub and the agent hub draw around themselves.

use theme::{Fg, Theme};
use tui::{truncate_to_width, visible_width};

use crate::components::bold;

pub(crate) fn top_border(t: &Theme, width: usize, title: &str) -> String {
    let label = format!(" {title} ");
    let fill = width.saturating_sub(2 + visible_width(&label));
    format!(
        "{}{}{}",
        t.fg(Fg::Border, "╭"),
        bold(&label),
        t.fg(Fg::Border, &format!("{}╮", "─".repeat(fill)))
    )
}

pub(crate) fn divider(t: &Theme, width: usize) -> String {
    t.fg(Fg::Border, &format!("├{}┤", "─".repeat(width.saturating_sub(2))))
}

pub(crate) fn bottom_border(t: &Theme, width: usize) -> String {
    t.fg(Fg::Border, &format!("╰{}╯", "─".repeat(width.saturating_sub(2))))
}

/// `content` between the side borders, cut to fit.
pub(crate) fn boxed_row(t: &Theme, content: &str, width: usize) -> String {
    let inner = width.saturating_sub(4);
    let cut = truncate_to_width(content, inner, "…", false);
    let pad = inner.saturating_sub(visible_width(&cut));
    format!(
        "{} {cut}{} {}",
        t.fg(Fg::Border, "│"),
        " ".repeat(pad),
        t.fg(Fg::Border, "│")
    )
}
