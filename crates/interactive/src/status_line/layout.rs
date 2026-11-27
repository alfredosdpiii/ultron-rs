//! Width fitting and group alignment for [`render_status_line`].

use tui::{truncate_to_width, visible_width};

use super::*;

type RenderedSide = Vec<(SegmentId, String)>;

struct RenderCtx<'a> {
    layout: &'a Layout,
    data: &'a StatusData,
    sym: &'a Symbols,
    style: &'a dyn StatusStyle,
}

fn group_width(parts: &[(SegmentId, String)], sep_w: usize) -> usize {
    if parts.is_empty() {
        return 0;
    }
    parts.iter().map(|(_, s)| visible_width(s)).sum::<usize>() + (parts.len() - 1) * (sep_w + 2) + 2
}

fn total_width(left: &[(SegmentId, String)], right: &[(SegmentId, String)], sep_w: (usize, usize)) -> usize {
    group_width(left, sep_w.0) + group_width(right, sep_w.1) + usize::from(!left.is_empty() && !right.is_empty())
}

fn shrink_session_name(
    left: &[(SegmentId, String)],
    right: &mut [(SegmentId, String)],
    sep_w: (usize, usize),
    width: usize,
) {
    let Some(i) = right.iter().position(|(id, _)| *id == SegmentId::SessionName) else {
        return;
    };
    let total = total_width(left, right, sep_w);
    if total <= width {
        return;
    }
    let cur = visible_width(&right[i].1);
    let shrink = cur.saturating_sub(8).min(total - width);
    if shrink > 0 {
        right[i].1 = truncate_to_width(&right[i].1, cur - shrink, "…", false);
    }
}

fn shrink_path_segment(
    left: &mut [(SegmentId, String)],
    right: &[(SegmentId, String)],
    sep_w: (usize, usize),
    width: usize,
    ctx: &RenderCtx<'_>,
) {
    let Some(i) = left.iter().position(|(id, _)| *id == SegmentId::Path) else {
        return;
    };
    let total = total_width(left, right, sep_w);
    if total <= width {
        return;
    }
    let over = total - width;
    let cur = visible_width(&left[i].1);
    let mut max = ctx.layout.options.path.max_length.min(cur).saturating_sub(over).max(4);
    let mut opts = ctx.layout.options.clone();
    loop {
        opts.path.max_length = max;
        let s = render_segment(SegmentId::Path, ctx.data, &opts, ctx.sym, ctx.style).unwrap_or_default();
        let saved = cur.saturating_sub(visible_width(&s));
        left[i].1 = s;
        if saved >= over || max == 4 {
            break;
        }
        max = max.saturating_sub(over - saved).max(4);
    }
}

fn fit_to_width(
    left: &mut RenderedSide,
    right: &mut RenderedSide,
    sep_w: (usize, usize),
    width: usize,
    ctx: &RenderCtx<'_>,
) {
    shrink_session_name(left, right, sep_w, width);
    while total_width(left, right, sep_w) > width && !right.is_empty() {
        right.pop();
    }
    shrink_path_segment(left, right, sep_w, width, ctx);
    while total_width(left, right, sep_w) > width && !left.is_empty() {
        let idx = left
            .iter()
            .rposition(|(id, _)| *id != SegmentId::Path)
            .unwrap_or(left.len() - 1);
        left.remove(idx);
    }
}

fn format_group(parts: &[(SegmentId, String)], sep: &str, style: &dyn StatusStyle) -> String {
    if parts.is_empty() {
        return String::new();
    }
    let sep = format!(" {} ", style.fg(Role::Separator, sep));
    format!(
        " {} ",
        parts.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(&sep)
    )
}

fn join_groups(lg: &str, rg: &str, width: usize) -> String {
    if lg.is_empty() && rg.is_empty() {
        return String::new();
    }
    if width == 0 || lg.is_empty() || rg.is_empty() {
        let mid = if !lg.is_empty() && !rg.is_empty() { " " } else { "" };
        if width > 0 && lg.is_empty() {
            return format!("{}{rg}", " ".repeat(width.saturating_sub(visible_width(rg))));
        }
        return format!("{lg}{mid}{rg}");
    }
    let gap = width.saturating_sub(visible_width(lg) + visible_width(rg)).max(1);
    format!("{lg}{}{rg}", " ".repeat(gap))
}

/// Lay out the status line in `width` columns (omp `#renderStatusLine`, transparent/plain layout: no fill, no caps).
pub fn render_status_line(
    layout: &Layout,
    d: &StatusData,
    sym: &Symbols,
    style: &dyn StatusStyle,
    width: usize,
) -> String {
    let render_side = |ids: &[SegmentId]| -> RenderedSide {
        ids.iter()
            .filter_map(|id| render_segment(*id, d, &layout.options, sym, style).map(|s| (*id, s)))
            .collect()
    };
    let mut left = render_side(&layout.left);
    let mut right = render_side(&layout.right);
    let (lsep, rsep) = sym.separator(layout.separator);
    let sep_w = (visible_width(lsep), visible_width(rsep));
    if width > 0 {
        let ctx = RenderCtx {
            layout,
            data: d,
            sym,
            style,
        };
        fit_to_width(&mut left, &mut right, sep_w, width, &ctx);
    }
    let lg = format_group(&left, lsep, style);
    let rg = format_group(&right, rsep, style);
    join_groups(&lg, &rg, width)
}
