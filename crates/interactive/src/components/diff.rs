//! Diff rendering with word-level inverse highlighting.

use jsrt::string;
use theme::{Fg, Theme};

use super::inverse;
use crate::word_diff::diff_words;

/// `^([+-\s])(\s*\d*)\s(.*)$` on one diff line: prefix, line number, content.
fn parse_diff_line(line: &str) -> Option<(char, &str, &str)> {
    let mut chars = line.chars();
    let prefix = chars
        .next()
        .filter(|c| *c == '+' || *c == '-' || string::is_space(*c))?;
    let rest = chars.as_str();
    let ws = rest.len() - rest.trim_start_matches(string::is_space).len();
    let digits = rest[ws..].bytes().take_while(u8::is_ascii_digit).count();
    let after = rest[ws + digits..].chars().next();
    let split = if after.is_some_and(string::is_space) {
        ws + digits
    } else if ws > 0 {
        rest[..ws].char_indices().last().map(|(i, _)| i)?
    } else {
        return None;
    };
    let sep = rest[split..].chars().next()?;
    let content = &rest[split + sep.len_utf8()..];
    if content.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((prefix, &rest[..split], content))
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start_matches(string::is_space).len()]
}

fn intra_line_diff(old: &str, new: &str) -> (String, String) {
    let (mut removed, mut added) = (String::new(), String::new());
    let (mut first_removed, mut first_added) = (true, true);
    for part in diff_words(old, new) {
        let (line, first) = if part.removed {
            (&mut removed, &mut first_removed)
        } else if part.added {
            (&mut added, &mut first_added)
        } else {
            removed.push_str(&part.value);
            added.push_str(&part.value);
            continue;
        };
        let mut value = part.value.as_str();
        if *first {
            let ws = leading_ws(value);
            line.push_str(ws);
            value = &value[ws.len()..];
            *first = false;
        }
        if !value.is_empty() {
            line.push_str(&inverse(value));
        }
    }
    (removed, added)
}

fn expand_tabs(s: &str) -> String {
    s.replace('\t', "   ")
}

fn take_diff_run<'a>(lines: &[&'a str], i: &mut usize, want: char) -> Vec<(&'a str, &'a str)> {
    let mut run = Vec::new();
    while let Some((p, n, c)) = lines.get(*i).and_then(|l| parse_diff_line(l)) {
        if p != want {
            break;
        }
        run.push((n, c));
        *i += 1;
    }
    run
}

fn push_change_block(theme: &Theme, out: &mut Vec<String>, removed: &[(&str, &str)], added: &[(&str, &str)]) {
    if let ([(rn, rc)], [(an, ac)]) = (removed, added) {
        let (r, a) = intra_line_diff(&expand_tabs(rc), &expand_tabs(ac));
        out.push(theme.fg(Fg::ToolDiffRemoved, &format!("-{rn} {r}")));
        out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{an} {a}")));
        return;
    }
    for (n, c) in removed {
        out.push(theme.fg(Fg::ToolDiffRemoved, &format!("-{n} {}", expand_tabs(c))));
    }
    for (n, c) in added {
        out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{n} {}", expand_tabs(c))));
    }
}

/// `renderDiff`: colored diff lines, with word-level inverse highlighting for a single changed line.
pub fn render_diff(theme: &Theme, diff: &str) -> String {
    let lines: Vec<&str> = diff.split('\n').collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some((prefix, num, content)) = parse_diff_line(lines[i]) else {
            out.push(theme.fg(Fg::ToolDiffContext, lines[i]));
            i += 1;
            continue;
        };
        if prefix == '-' {
            let removed = take_diff_run(&lines, &mut i, '-');
            let added = take_diff_run(&lines, &mut i, '+');
            push_change_block(theme, &mut out, &removed, &added);
        } else if prefix == '+' {
            out.push(theme.fg(Fg::ToolDiffAdded, &format!("+{num} {}", expand_tabs(content))));
            i += 1;
        } else {
            out.push(theme.fg(Fg::ToolDiffContext, &format!(" {num} {}", expand_tabs(content))));
            i += 1;
        }
    }
    out.join("\n")
}
