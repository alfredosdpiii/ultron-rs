//! Per-segment rendering for [`super::render_status_line`].

use super::*;

type SegmentFn = fn(&StatusData, &SegmentOptions, &Symbols, &dyn StatusStyle) -> Option<String>;

const RENDERERS: [(SegmentId, SegmentFn); 24] = [
    (SegmentId::Pi, render_pi),
    (SegmentId::Status, render_status),
    (SegmentId::Model, render_model),
    (SegmentId::Path, render_path),
    (SegmentId::Git, render_git),
    (SegmentId::Pr, render_pr),
    (SegmentId::Subagents, render_subagents),
    (SegmentId::TokenIn, render_token_in),
    (SegmentId::TokenOut, render_token_out),
    (SegmentId::TokenTotal, render_token_total),
    (SegmentId::TokenRate, render_token_rate),
    (SegmentId::Cost, render_cost),
    (SegmentId::ContextPct, render_context_pct),
    (SegmentId::ContextTotal, render_context_total),
    (SegmentId::TimeSpent, render_time_spent),
    (SegmentId::Time, render_time),
    (SegmentId::Session, render_session),
    (SegmentId::Hostname, render_hostname),
    (SegmentId::CacheRead, render_cache_read),
    (SegmentId::CacheWrite, render_cache_write),
    (SegmentId::CacheHit, render_cache_hit),
    (SegmentId::SessionName, render_session_name),
    (SegmentId::Vim, render_vim),
    (SegmentId::State, render_state),
];

/// Render one segment; `None` when hidden.
pub fn render_segment(
    id: SegmentId,
    d: &StatusData,
    opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    let (entry_id, render) = RENDERERS[id as usize];
    debug_assert_eq!(entry_id, id);
    render(d, opts, sym, style)
}

fn stat(value: u64, icon: &str, role: Role, style: &dyn StatusStyle) -> Option<String> {
    (value > 0).then(|| style.fg(role, &with_icon(icon, &format_number(value))))
}

fn render_pi(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let content = match d.turn_elapsed_ms {
        Some(ms) => {
            let frame = sym.spinner[(d.now_ms / 80) as usize % sym.spinner.len()];
            let s = ms / 1000;
            let timer = if s < 60 {
                format!("{s}s")
            } else if s < 3600 {
                format!("{}m", s / 60)
            } else {
                format!("{}h", (s / 3600).min(99))
            };
            format!("{frame} {timer}")
        }
        None => sym.brand.to_string(),
    };
    (!content.is_empty()).then(|| style.fg(Role::Dim, &content))
}

fn render_status(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let text: Vec<String> = d
        .hook_statuses
        .iter()
        .map(|s| sanitize_status_text(s))
        .filter(|s| !s.is_empty())
        .collect();
    (!text.is_empty()).then(|| style.fg(Role::Accent, &text.join(sym.dot)))
}

fn render_model(d: &StatusData, opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let name = d.model_name.as_deref().filter(|n| !n.is_empty()).unwrap_or("no-model");
    let name = name.strip_prefix("Claude ").unwrap_or(name);
    let mut content = style.fg(Role::Model, &with_icon(sym.model, name));
    if opts.show_thinking_level
        && let Some(level) = &d.thinking_level
    {
        content += &style.fg(Role::Model, &format!("{}{}", sym.dot, thinking_display(level, sym)));
    }
    Some(content)
}

fn render_path(d: &StatusData, opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    Some(style.fg(
        Role::Path,
        &with_icon(sym.folder, &display_path(&d.cwd, &d.home, &opts.path)),
    ))
}

fn git_indicators(s: GitCounts, g: &GitOptions, style: &dyn StatusStyle) -> Vec<String> {
    let mut ind = Vec::new();
    if g.show_unstaged && s.unstaged > 0 {
        ind.push(style.fg(Role::Dirty, &format!("*{}", s.unstaged)));
    }
    if g.show_staged && s.staged > 0 {
        ind.push(style.fg(Role::Staged, &format!("+{}", s.staged)));
    }
    if g.show_untracked && s.untracked > 0 {
        ind.push(style.fg(Role::Untracked, &format!("?{}", s.untracked)));
    }
    ind
}

fn is_git_dirty(s: GitCounts) -> bool {
    s.staged > 0 || s.unstaged > 0 || s.untracked > 0
}

fn append_git_indicators(content: String, text: String, show_branch: bool, icon: &str) -> String {
    if content.is_empty() && !show_branch {
        with_icon(icon, &text)
    } else if content.is_empty() {
        text
    } else {
        format!("{content} {text}")
    }
}

fn render_git(d: &StatusData, opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let g = &opts.git;
    if d.git_branch.is_none() && d.git_status.is_none() {
        return None;
    }
    let mut content = match (&d.git_branch, g.show_branch) {
        (Some(b), true) => with_icon(sym.branch, b),
        _ => String::new(),
    };
    let mut dirty = false;
    if let Some(s) = d.git_status {
        dirty = is_git_dirty(s);
        let ind = git_indicators(s, g, style);
        if !ind.is_empty() {
            content = append_git_indicators(content, ind.join(" "), g.show_branch, sym.git);
        }
    }
    let role = if dirty { Role::GitDirty } else { Role::GitClean };
    (!content.is_empty()).then(|| style.fg(role, &content))
}

fn render_pr(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    d.pr.as_ref().map(|(number, url)| {
        let label = with_icon(sym.pr, &format!("#{number}"));
        let content = if d.hyperlinks {
            format!("\x1b]8;;{url}\x07{label}\x1b]8;;\x07")
        } else {
            label
        };
        style.fg(Role::Accent, &content)
    })
}

fn render_subagents(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    (d.subagent_count > 0).then(|| style.fg(Role::Subagents, &with_icon(sym.agents, &d.subagent_count.to_string())))
}

fn render_token_in(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    stat(d.usage.input, sym.input, Role::Spend, style)
}

fn render_token_out(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    stat(d.usage.output, sym.output, Role::Output, style)
}

fn render_token_total(
    d: &StatusData,
    _opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    let u = &d.usage;
    stat(u.input + u.output + u.cache_write, sym.tokens, Role::Spend, style)
}

fn render_token_rate(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    d.usage
        .tokens_per_second
        .filter(|r| *r > 0.0)
        .map(|r| style.fg(Role::Output, &with_icon(sym.throughput, &format!("{r:.1} tok/s"))))
}

fn render_cost(d: &StatusData, _opts: &SegmentOptions, _sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let own = (d.usage.cost - d.usage.subagent_cost).max(0.0);
    format_billing(own, d.usage.subagent_cost, d.using_subscription).map(|b| style.fg(Role::Cost, &b))
}

fn render_context_pct(
    d: &StatusData,
    _opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    let role = match context_level(d.context_percent.unwrap_or(0.0), d.context_window) {
        ContextLevel::Normal => Role::Context,
        ContextLevel::Warning => Role::Warning,
        ContextLevel::Purple => Role::Purple,
        ContextLevel::Error => Role::Error,
    };
    let text = if d.context_percent.is_none() && d.context_window > 0 {
        format_number(d.context_window)
    } else {
        format_context_usage(d.context_percent, d.context_window, d.context_tokens)
    };
    let mut text = style.fg(role, &text);
    if d.auto_compact && !sym.auto.is_empty() {
        text = format!("{text} {}", style.fg(role, sym.auto));
    }
    Some(with_icon(sym.context, &text))
}

fn render_context_total(
    d: &StatusData,
    _opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    (d.context_window > 0).then(|| style.fg(Role::Context, &with_icon(sym.context, &format_number(d.context_window))))
}

fn render_time_spent(
    d: &StatusData,
    _opts: &SegmentOptions,
    sym: &Symbols,
    _style: &dyn StatusStyle,
) -> Option<String> {
    (d.active_ms >= 1000).then(|| with_icon(sym.time, &format_duration(d.active_ms)))
}

fn render_time(d: &StatusData, opts: &SegmentOptions, sym: &Symbols, _style: &dyn StatusStyle) -> Option<String> {
    let (mut h, m, s) = d.clock;
    let mut suffix = "";
    if opts.time.twelve_hour {
        suffix = if h >= 12 { "pm" } else { "am" };
        h = if h % 12 == 0 { 12 } else { h % 12 };
    }
    let mut t = format!("{h}:{m:02}");
    if opts.time.show_seconds {
        t += &format!(":{s:02}");
    }
    Some(with_icon(sym.time, &(t + suffix)))
}

fn render_session(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, _style: &dyn StatusStyle) -> Option<String> {
    let id: String = d.session_id.as_deref().unwrap_or("").chars().take(8).collect();
    Some(with_icon(sym.session, if id.is_empty() { "new" } else { &id }))
}

fn render_hostname(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, _style: &dyn StatusStyle) -> Option<String> {
    Some(with_icon(sym.host, d.hostname.split('.').next().unwrap_or("")))
}

fn render_cache_read(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    stat(d.usage.cache_read, sym.cache, Role::Spend, style)
}

fn render_cache_write(
    d: &StatusData,
    _opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    stat(d.usage.cache_write, sym.cache, Role::Output, style)
}

fn render_cache_hit(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let u = &d.usage;
    (u.cache_read > 0).then(|| {
        let rate = u.cache_read as f64 / (u.cache_read + u.cache_write + u.input) as f64 * 100.0;
        format!("{} {}", sym.cache, style.fg(Role::Spend, &format!("{rate:.2}%")))
    })
}

fn render_session_name(
    d: &StatusData,
    _opts: &SegmentOptions,
    _sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    d.session_name
        .as_deref()
        .map(sanitize_status_text)
        .filter(|s| !s.is_empty())
        .map(|s| style.fg(Role::Accent, &s))
}

fn render_vim(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let v = d.vim.as_ref().filter(|v| v.display != VimDisplay::None)?;
    let (idx, label, role) = match v.mode {
        VimMode::Insert => (0, "INSERT", Role::Success),
        VimMode::Normal => (1, "NORMAL", Role::Accent),
        VimMode::Visual => (2, "VISUAL", Role::Warning),
        VimMode::VisualLine => (3, "V-LINE", Role::Warning),
        VimMode::Replace => (4, "REPLACE", Role::Accent),
    };
    let mut label = if v.display == VimDisplay::Icon {
        sym.vim[idx].to_string()
    } else {
        label.to_string()
    };
    if v.selected_lines > 1 {
        label += &format!(" {}L", v.selected_lines);
    }
    let pending = if v.pending.is_empty() {
        String::new()
    } else {
        style.fg(Role::Muted, &format!(" {}", v.pending))
    };
    Some(format!("{}{pending}", style.fg(role, &label)))
}

fn render_state(d: &StatusData, _opts: &SegmentOptions, sym: &Symbols, style: &dyn StatusStyle) -> Option<String> {
    let (text, role) = match d.stream {
        StreamState::Idle => return None,
        StreamState::Thinking => ("thinking", Role::Purple),
        StreamState::Streaming => ("streaming", Role::Accent),
        StreamState::Running => ("running", Role::Success),
        StreamState::Waiting => ("waiting", Role::Warning),
    };
    let frame = sym.spinner[(d.now_ms / 80) as usize % sym.spinner.len()];
    Some(style.fg(role, &format!("{frame} {text}")))
}
