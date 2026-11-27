use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::utils::visible_width;
use jsrt::string;

pub(super) const NAMED_OPERATOR_START: char = '\u{f0004}';
pub(super) const NAMED_OPERATOR_END: char = '\u{f0005}';
pub(super) const LAYOUT_MARKER_START: char = '\u{f0000}';
pub(super) const LAYOUT_MARKER_END: char = '\u{f0001}';

static SIMPLE_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}.]+$").expect("regex"));

pub(super) fn is_simple(value: &str) -> bool {
    SIMPLE_VALUE.is_match(value).unwrap_or(false)
}

fn is_letter_or_number(c: char) -> bool {
    let mut b = [0u8; 4];
    is_simple(c.encode_utf8(&mut b)) && c != '.'
}

fn strip_named_op_starts(chars: &[char]) -> Vec<char> {
    let mut out = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == NAMED_OPERATOR_START {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            if prev.is_some_and(|p| is_letter_or_number(p) || matches!(p, ')' | ']' | '}' | LAYOUT_MARKER_END)) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn strip_named_op_ends(chars: &[char]) -> String {
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == NAMED_OPERATOR_END {
            let next = chars.get(i + 1).copied();
            if next.is_some_and(|n| is_letter_or_number(n) || n == '√' || n == LAYOUT_MARKER_START) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn collapse_line_spaces(line: &str) -> String {
    let mut out = String::new();
    let mut in_space = false;
    for c in line.chars() {
        if c == ' ' || c == '\t' {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    string::trim(&out).to_string()
}

pub(super) fn normalize_output(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let s = strip_named_op_starts(&chars);
    let t = strip_named_op_ends(&s);
    let mut lines: Vec<String> = t.split('\n').map(collapse_line_spaces).collect();
    if lines.first().is_some_and(String::is_empty) {
        lines.remove(0);
    }
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    string::trim(&lines.join("\n")).to_string()
}

pub(super) enum Node {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Script {
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

pub(super) struct Layout {
    pub lines: Vec<String>,
    pub width: usize,
    pub baseline: usize,
}

fn text_layout(text: String) -> Layout {
    Layout {
        width: visible_width(&text),
        lines: vec![text],
        baseline: 0,
    }
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{line}{}", " ".repeat(left), " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len() as isize - l.baseline as isize - 1)
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut row: isize = 0;
    while row <= baseline as isize + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line += &pad_layout_line(&layout.lines[source_row as usize], layout.width, false);
            } else {
                line += &" ".repeat(layout.width);
            }
        }
        lines.push(line.trim_end_matches(string::is_space).to_string());
        row += 1;
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// Finds `\u{f0000}(\d+)\u{f0001}` markers: (start byte, end byte, index).
fn layout_markers(line: &str) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = line[from..].find(LAYOUT_MARKER_START) {
        let start = from + off;
        let digits_start = start + LAYOUT_MARKER_START.len_utf8();
        let digits_len = line[digits_start..].bytes().take_while(u8::is_ascii_digit).count();
        let digits_end = digits_start + digits_len;
        if digits_len > 0 && line[digits_end..].starts_with(LAYOUT_MARKER_END) {
            let end = digits_end + LAYOUT_MARKER_END.len_utf8();
            out.push((start, end, line[digits_start..digits_end].parse().unwrap_or(usize::MAX)));
            from = end;
        } else {
            from = digits_start;
        }
    }
    out
}

fn gap_text_layout(sliced: &str, previous: Option<&Node>, next: &Node) -> Layout {
    let trimmed = (if previous.is_some() {
        sliced.trim_start_matches(string::is_space)
    } else {
        sliced
    })
    .trim_end_matches(string::is_space);
    let lead = matches!(previous, Some(Node::Matrix { .. })) && sliced.starts_with(string::is_space);
    let trail = matches!(next, Node::Matrix { .. }) && sliced.ends_with(string::is_space);
    let text = if !trimmed.is_empty() {
        format!(
            "{}{trimmed}{}",
            if lead { " " } else { "" },
            if trail { " " } else { "" }
        )
    } else if lead || trail {
        " ".to_string()
    } else {
        String::new()
    };
    text_layout(text)
}

fn trailing_text_layout(sliced: &str, previous: Option<&Node>) -> Layout {
    let trimmed = if previous.is_some() {
        sliced.trim_start_matches(string::is_space)
    } else {
        sliced
    };
    let text = if matches!(previous, Some(Node::Matrix { .. })) && sliced.starts_with(string::is_space) {
        format!(" {trimmed}")
    } else {
        trimmed.to_string()
    };
    text_layout(text)
}

fn render_fraction_node(numerator: &str, denominator: &str, nodes: &[Node]) -> Layout {
    let numerator = render_layout(numerator, nodes);
    let denominator = render_layout(denominator, nodes);
    let content = numerator.width.max(denominator.width).max(1);
    let width = content + 2;
    let mut lines: Vec<String> = numerator
        .lines
        .iter()
        .map(|l| pad_layout_line(l, width, true))
        .collect();
    lines.push(format!(" {} ", "─".repeat(content)));
    lines.extend(denominator.lines.iter().map(|l| pad_layout_line(l, width, true)));
    Layout {
        lines,
        width,
        baseline: numerator.lines.len(),
    }
}

fn render_operator_node(operator: &str, lower: Option<&str>, upper: Option<&str>) -> Layout {
    let content = visible_width(operator)
        .max(lower.map_or(0, visible_width))
        .max(upper.map_or(0, visible_width));
    let mut lines = Vec::new();
    if let Some(u) = upper {
        lines.push(format!("{} ", pad_layout_line(u, content, true)));
    }
    lines.push(format!("{} ", pad_layout_line(operator, content, true)));
    if let Some(l) = lower {
        lines.push(format!("{} ", pad_layout_line(l, content, true)));
    }
    Layout {
        lines,
        width: content + 1,
        baseline: usize::from(upper.is_some()),
    }
}

fn render_script_node(lower: Option<&str>, upper: Option<&str>, nodes: &[Node]) -> Layout {
    let upper = upper.map(|u| render_layout(u, nodes));
    let lower = lower.map(|l| render_layout(l, nodes));
    let width = upper
        .as_ref()
        .map_or(0, |u| u.width)
        .max(lower.as_ref().map_or(0, |l| l.width));
    let mut lines = Vec::new();
    if let Some(u) = &upper {
        lines.extend(u.lines.iter().map(|l| pad_layout_line(l, width, false)));
    }
    lines.push(" ".repeat(width));
    if let Some(l) = &lower {
        lines.extend(l.lines.iter().map(|x| pad_layout_line(x, width, false)));
    }
    let baseline = upper.as_ref().map_or(0, |u| u.lines.len());
    Layout { lines, width, baseline }
}

fn render_matrix_node(lines: &[String], baseline: usize) -> Layout {
    let width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    let lines = lines.iter().map(|l| pad_layout_line(l, width, false)).collect();
    Layout { lines, width, baseline }
}

fn render_node(node: &Node, nodes: &[Node]) -> Layout {
    match node {
        Node::Fraction { numerator, denominator } => render_fraction_node(numerator, denominator, nodes),
        Node::Operator { operator, lower, upper } => render_operator_node(operator, lower.as_deref(), upper.as_deref()),
        Node::Script { lower, upper } => render_script_node(lower.as_deref(), upper.as_deref(), nodes),
        Node::Matrix { lines, baseline } => render_matrix_node(lines, *baseline),
    }
}

fn render_layout_line(source_line: &str, nodes: &[Node]) -> Layout {
    let mut layouts = Vec::new();
    let mut position = 0;
    let mut previous: Option<&Node> = None;
    for (index, end, node_index) in layout_markers(source_line) {
        let Some(node) = nodes.get(node_index) else { continue };
        if index > position {
            layouts.push(gap_text_layout(&source_line[position..index], previous, node));
        }
        layouts.push(render_node(node, nodes));
        position = end;
        previous = Some(node);
    }
    if position < source_line.len() {
        layouts.push(trailing_text_layout(&source_line[position..], previous));
    }
    join_layouts(&layouts)
}

pub(super) fn render_layout(source: &str, nodes: &[Node]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0;
    for source_line in source.split('\n') {
        let line_layout = render_layout_line(source_line, nodes);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    let width = rendered_lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

pub(super) fn marker(index: usize) -> String {
    format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
}

/// `/\u{f0000}(\d+)\u{f0001}$/` on the result so far.
pub(super) fn trailing_marker(result: &str) -> Option<usize> {
    let rest = result.strip_suffix(LAYOUT_MARKER_END)?;
    let digits = rest.len() - rest.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (head, num) = rest.split_at(rest.len() - digits);
    if !head.ends_with(LAYOUT_MARKER_START) {
        return None;
    }
    num.parse().ok()
}
