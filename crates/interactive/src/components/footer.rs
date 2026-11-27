//! Interactive TUI footer formatting and rendering.

use jsrt::{number, string};
use theme::{Fg, Theme};
use tui::{truncate_to_width, visible_width};

use super::bold;

/// The model as the footer shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterModel {
    pub id: String,
    pub provider: String,
    pub reasoning: bool,
    pub context_window: u64,
}

/// What `FooterComponent` reads from the session: usage totals summed over all entries, the latest assistant cache
/// hit rate, context usage, and the footer-data provider's branch, provider count and extension statuses.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FooterView {
    pub cwd: String,
    pub home: Option<String>,
    pub git_branch: Option<String>,
    pub session_name: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub latest_cache_hit_rate: Option<f64>,
    /// `None` when there is no context usage; `Some(None)` when its percent is unknown (after compaction).
    pub context_percent: Option<Option<f64>>,
    /// `contextUsage.contextWindow` when known.
    pub context_window: Option<u64>,
    pub model: Option<FooterModel>,
    pub thinking_level: String,
    pub using_subscription: bool,
    pub provider_count: usize,
    pub auto_compact: bool,
    pub experimental: bool,
    /// Extension statuses by key.
    pub statuses: Vec<(String, String)>,
}

pub fn format_tokens(count: u64) -> String {
    let c = count as f64;
    match count {
        0..1_000 => count.to_string(),
        1_000..10_000 => format!("{}k", number::to_fixed(c / 1000.0, 1)),
        10_000..1_000_000 => format!("{}k", (c / 1000.0).round()),
        1_000_000..10_000_000 => format!("{}M", number::to_fixed(c / 1_000_000.0, 1)),
        _ => format!("{}M", (c / 1_000_000.0).round()),
    }
}

/// `formatCwdForFooter` for absolute POSIX paths: `~` for the home directory and paths under it.
pub fn format_cwd_for_footer(cwd: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|h| !h.is_empty()) else {
        return cwd.to_string();
    };
    let norm = |p: &str| {
        let t = p.trim_end_matches('/');
        if t.is_empty() { "/".to_string() } else { t.to_string() }
    };
    let (c, h) = (norm(cwd), norm(home));
    if c == h {
        return "~".to_string();
    }
    let base = if h == "/" { String::new() } else { h };
    match c.strip_prefix(&format!("{base}/")) {
        Some(rest) => format!("~/{rest}"),
        None => cwd.to_string(),
    }
}

fn sanitize_status(text: &str) -> String {
    let spaced = text.replace(['\r', '\n', '\t'], " ");
    let mut out = String::new();
    for c in spaced.chars() {
        if !(c == ' ' && out.ends_with(' ')) {
            out.push(c);
        }
    }
    out.trim_matches(string::is_space).to_string()
}

fn footer_pwd(v: &FooterView) -> String {
    let mut pwd = format_cwd_for_footer(&v.cwd, v.home.as_deref());
    if let Some(b) = v.git_branch.as_deref().filter(|b| !b.is_empty()) {
        pwd = format!("{pwd} ({b})");
    }
    if let Some(n) = v.session_name.as_deref().filter(|n| !n.is_empty()) {
        pwd = format!("{pwd} • {n}");
    }
    pwd
}

fn footer_context_display(theme: &Theme, v: &FooterView, claude_code: bool) -> String {
    let window = v
        .context_window
        .or(v.model.as_ref().map(|m| m.context_window))
        .unwrap_or(0);
    let percent_value = v.context_percent.flatten().unwrap_or(0.0);
    let auto = if v.auto_compact && !claude_code { " (auto)" } else { "" };
    let display = match v.context_percent {
        Some(None) => format!("?/{}{auto}", format_tokens(window)),
        _ => format!(
            "{}%/{}{auto}",
            number::to_fixed(percent_value, 1),
            format_tokens(window)
        ),
    };
    if percent_value > 90.0 {
        theme.fg(Fg::Error, &display)
    } else if percent_value > 70.0 {
        theme.fg(Fg::Warning, &display)
    } else {
        display
    }
}

fn footer_stats(theme: &Theme, v: &FooterView, claude_code: bool) -> String {
    let mut stats = Vec::new();
    for (n, tag) in [
        (v.input, "↑"),
        (v.output, "↓"),
        (v.cache_read, "R"),
        (v.cache_write, "W"),
    ] {
        if n > 0 {
            stats.push(format!("{tag}{}", format_tokens(n)));
        }
    }
    if let Some(rate) = v
        .latest_cache_hit_rate
        .filter(|_| v.cache_read > 0 || v.cache_write > 0)
    {
        stats.push(format!("CH{}%", number::to_fixed(rate, 1)));
    }
    let subscription = v
        .model
        .as_ref()
        .is_some_and(|m| m.provider == config::providers::KIMI_CODING || v.using_subscription);
    if v.cost != 0.0 || subscription {
        let suffix = if subscription { " (sub)" } else { "" };
        stats.push(format!("${}{suffix}", number::to_fixed(v.cost, 3)));
    }
    stats.push(footer_context_display(theme, v, claude_code));
    if v.experimental {
        stats.push(format!(
            "{} {}",
            theme.fg(Fg::Dim, "•"),
            bold(&theme.fg(Fg::Warning, "xp"))
        ));
    }
    stats.join(" ")
}

fn footer_model_name(model: Option<&FooterModel>, claude_code: bool) -> String {
    match model {
        Some(m) if !m.id.is_empty() && claude_code => format!("{} (Claude Code)", m.id),
        Some(m) if !m.id.is_empty() => m.id.clone(),
        _ => "no-model".to_string(),
    }
}

fn footer_right_text(v: &FooterView, claude_code: bool, left_width: usize, width: usize) -> String {
    let model = v.model.as_ref();
    let model_name = footer_model_name(model, claude_code);
    let right_plain = if model.is_some_and(|m| m.reasoning) {
        let level = if v.thinking_level.is_empty() {
            "off"
        } else {
            v.thinking_level.as_str()
        };
        if level == "off" {
            format!("{model_name} • thinking off")
        } else {
            format!("{model_name} • {level}")
        }
    } else {
        model_name
    };
    let Some(m) = model.filter(|_| v.provider_count > 1 && !claude_code) else {
        return right_plain;
    };
    let with_provider = format!("({}) {right_plain}", m.provider);
    if left_width + 2 + visible_width(&with_provider) > width {
        right_plain
    } else {
        with_provider
    }
}

fn compose_footer_stats_line(theme: &Theme, left: &str, left_width: usize, right: &str, width: usize) -> String {
    let right_width = visible_width(right);
    let line = if left_width + 2 + right_width <= width {
        format!("{left}{}{right}", " ".repeat(width - left_width - right_width))
    } else if width > left_width + 2 {
        let r = truncate_to_width(right, width - left_width - 2, "", false);
        format!(
            "{left}{}{r}",
            " ".repeat(width.saturating_sub(left_width + visible_width(&r)))
        )
    } else {
        left.to_string()
    };
    let remainder = &line[left.len()..];
    format!("{}{}", theme.fg(Fg::Dim, left), theme.fg(Fg::Dim, remainder))
}

fn footer_statuses_line(theme: &Theme, statuses: &[(String, String)], width: usize) -> Option<String> {
    if statuses.is_empty() {
        return None;
    }
    let mut sorted = statuses.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let line = sorted
        .iter()
        .map(|(_, t)| sanitize_status(t))
        .collect::<Vec<_>>()
        .join(" ");
    let ellipsis = theme.fg(Fg::Dim, "...");
    Some(truncate_to_width(&line, width, &ellipsis, false))
}

/// `FooterComponent.render`: the cwd line, the stats line with the model on the right, and extension statuses.
pub fn render_footer(theme: &Theme, v: &FooterView, width: usize) -> Vec<String> {
    let dim = |s: &str| theme.fg(Fg::Dim, s);
    let pwd = footer_pwd(v);
    let claude_code = v
        .model
        .as_ref()
        .is_some_and(|m| m.provider == config::providers::CLAUDE_CODE);
    let mut left = footer_stats(theme, v, claude_code);
    let mut left_width = visible_width(&left);
    if left_width > width {
        left = truncate_to_width(&left, width, "...", false);
        left_width = visible_width(&left);
    }
    let right = footer_right_text(v, claude_code, left_width, width);
    let mut lines = vec![
        truncate_to_width(&dim(&pwd), width, &dim("..."), false),
        compose_footer_stats_line(theme, &left, left_width, &right, width),
    ];
    if let Some(status_line) = footer_statuses_line(theme, &v.statuses, width) {
        lines.push(status_line);
    }
    lines
}
