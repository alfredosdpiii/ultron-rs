//! Text diffs from jsdiff 8.0.4 (BSD-3-Clause, Copyright (c) 2009-2015 Kevin Decker): the Myers diff of `Diff.diff`
//! over any tokens ([`diff_tokens`]), `diffLines` ([`diff_lines`]), and the display diff the TypeScript edit tool
//! stores in a tool result's `details.diff` (`generateDiffString` of core/tools/edit-diff.ts,
//! [`generate_diff_string`]), which the interactive mode's `renderDiff` draws.

/// One change object: `added`/`removed` flags and the joined text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub value: String,
    pub added: bool,
    pub removed: bool,
}

struct Node {
    count: usize,
    added: bool,
    removed: bool,
    prev: Option<usize>,
}

#[derive(Clone, Copy)]
struct Path {
    old_pos: isize,
    last: Option<usize>,
}

struct Myers<'a> {
    nodes: Vec<Node>,
    old: &'a [String],
    new: &'a [String],
    equals: fn(&str, &str) -> bool,
    join: fn(&[String]) -> String,
}

impl Myers<'_> {
    fn add_to_path(&mut self, path: Path, added: bool, removed: bool, inc: isize) -> Path {
        let node = match path.last.map(|i| &self.nodes[i]) {
            Some(l) if l.added == added && l.removed == removed => Node {
                count: l.count + 1,
                added,
                removed,
                prev: l.prev,
            },
            _ => Node {
                count: 1,
                added,
                removed,
                prev: path.last,
            },
        };
        self.nodes.push(node);
        Path {
            old_pos: path.old_pos + inc,
            last: Some(self.nodes.len() - 1),
        }
    }

    fn extract_common(&mut self, path: &mut Path, diagonal: isize) -> isize {
        let (new_len, old_len) = (self.new.len() as isize, self.old.len() as isize);
        let mut old_pos = path.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common = 0;
        while new_pos + 1 < new_len
            && old_pos + 1 < old_len
            && (self.equals)(&self.old[(old_pos + 1) as usize], &self.new[(new_pos + 1) as usize])
        {
            new_pos += 1;
            old_pos += 1;
            common += 1;
        }
        if common > 0 {
            self.nodes.push(Node {
                count: common,
                added: false,
                removed: false,
                prev: path.last,
            });
            path.last = Some(self.nodes.len() - 1);
        }
        path.old_pos = old_pos;
        new_pos
    }

    fn build(&self, last: Option<usize>) -> Vec<Change> {
        let mut chain = Vec::new();
        let mut cur = last;
        while let Some(i) = cur {
            chain.push(i);
            cur = self.nodes[i].prev;
        }
        chain.reverse();
        let (mut new_pos, mut old_pos) = (0, 0);
        chain
            .into_iter()
            .map(|i| {
                let n = &self.nodes[i];
                let value = if n.removed {
                    old_pos += n.count;
                    (self.join)(&self.old[old_pos - n.count..old_pos])
                } else {
                    new_pos += n.count;
                    if !n.added {
                        old_pos += n.count;
                    }
                    (self.join)(&self.new[new_pos - n.count..new_pos])
                };
                Change {
                    value,
                    added: n.added,
                    removed: n.removed,
                }
            })
            .collect()
    }

    fn select_base(&mut self, add: Option<Path>, remove: Option<Path>, diagonal: isize) -> Option<Path> {
        let new_len = self.new.len() as isize;
        let old_len = self.old.len() as isize;
        let add_path = add.filter(|path| (0..new_len).contains(&(path.old_pos - diagonal)));
        let remove_path = remove.filter(|path| path.old_pos + 1 < old_len);
        match (add_path, remove_path) {
            (Some(a), Some(r)) if r.old_pos < a.old_pos => Some(self.add_to_path(a, true, false, 0)),
            (_, Some(r)) => Some(self.add_to_path(r, false, true, 1)),
            (Some(a), None) => Some(self.add_to_path(a, true, false, 0)),
            (None, None) => None,
        }
    }

    fn step_edit(
        &mut self,
        best: &mut [Option<Path>],
        offset: isize,
        edit: isize,
        min_diag: &mut isize,
        max_diag: &mut isize,
    ) -> Option<Vec<Change>> {
        let (new_len, old_len) = (self.new.len() as isize, self.old.len() as isize);
        let idx = |diag: isize| (diag + offset) as usize;
        let mut diagonal = (*min_diag).max(-edit);
        while diagonal <= (*max_diag).min(edit) {
            let remove = best[idx(diagonal - 1)].take();
            let add = best[idx(diagonal + 1)];
            let Some(mut base) = self.select_base(add, remove, diagonal) else {
                best[idx(diagonal)] = None;
                diagonal += 2;
                continue;
            };
            let new_pos = self.extract_common(&mut base, diagonal);
            if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                return Some(self.build(base.last));
            }
            best[idx(diagonal)] = Some(base);
            if base.old_pos + 1 >= old_len {
                *max_diag = (*max_diag).min(diagonal - 1);
            }
            if new_pos + 1 >= new_len {
                *min_diag = (*min_diag).max(diagonal + 1);
            }
            diagonal += 2;
        }
        None
    }

    fn run(&mut self) -> Vec<Change> {
        let (new_len, old_len) = (self.new.len() as isize, self.old.len() as isize);
        let max_edit = new_len + old_len;
        let offset = max_edit + 1;
        let mut best: Vec<Option<Path>> = vec![None; (2 * offset + 1) as usize];
        let mut first = Path {
            old_pos: -1,
            last: None,
        };
        let new_pos = self.extract_common(&mut first, 0);
        if first.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
            return self.build(first.last);
        }
        best[offset as usize] = Some(first);
        let (mut min_diag, mut max_diag) = (isize::MIN, isize::MAX);
        for edit in 1..=max_edit {
            if let Some(changes) = self.step_edit(&mut best, offset, edit, &mut min_diag, &mut max_diag) {
                return changes;
            }
        }
        Vec::new()
    }
}

/// `Diff.diff` over `old` and `new` tokens (empty tokens already removed) with the subclass's `equals` and `join`.
pub fn diff_tokens(
    old: &[String],
    new: &[String],
    equals: fn(&str, &str) -> bool,
    join: fn(&[String]) -> String,
) -> Vec<Change> {
    Myers {
        nodes: Vec::new(),
        old,
        new,
        equals,
        join,
    }
    .run()
}

/// `lineDiff.tokenize` without options: lines with their `\n` (a `\r` stays on its line), no final empty line.
fn line_tokens(value: &str) -> Vec<String> {
    let mut lines: Vec<String> = value.split_inclusive('\n').map(str::to_string).collect();
    lines.retain(|line| !line.is_empty());
    lines
}

/// `diffLines(old, new)` without options.
pub fn diff_lines(old: &str, new: &str) -> Vec<Change> {
    diff_tokens(
        &line_tokens(old),
        &line_tokens(new),
        |a, b| a == b,
        <[std::string::String]>::concat,
    )
}

struct DiffFormatter {
    output: Vec<String>,
    width: usize,
    skip: String,
    context_lines: usize,
    old_line: usize,
    new_line: usize,
    first_changed: Option<usize>,
}

impl DiffFormatter {
    fn new(max_line: usize, context_lines: usize) -> Self {
        let width = max_line.to_string().len();
        Self {
            output: Vec::new(),
            width,
            skip: format!(" {} ...", " ".repeat(width)),
            context_lines,
            old_line: 1,
            new_line: 1,
            first_changed: None,
        }
    }

    fn push_change(&mut self, lines: &[&str], added: bool) {
        self.first_changed.get_or_insert(self.new_line);
        let width = self.width;
        for line in lines {
            if added {
                self.output.push(format!("+{:>width$} {line}", self.new_line));
                self.new_line += 1;
            } else {
                self.output.push(format!("-{:>width$} {line}", self.old_line));
                self.old_line += 1;
            }
        }
    }

    fn push_context(&mut self, lines: &[&str]) {
        let width = self.width;
        for line in lines {
            self.output.push(format!(" {:>width$} {line}", self.old_line));
            self.old_line += 1;
            self.new_line += 1;
        }
    }

    fn push_skip(&mut self, skipped: usize) {
        if skipped > 0 {
            self.output.push(self.skip.clone());
            self.old_line += skipped;
            self.new_line += skipped;
        }
    }

    fn push_unchanged(&mut self, lines: &[&str], last_was_change: bool, next_is_change: bool) {
        match (last_was_change, next_is_change) {
            (true, true) if lines.len() <= self.context_lines * 2 => self.push_context(lines),
            (true, true) => {
                let skipped = lines.len() - 2 * self.context_lines;
                self.push_context(&lines[..self.context_lines]);
                self.push_skip(skipped);
                self.push_context(&lines[lines.len() - self.context_lines..]);
            }
            (true, false) => {
                let shown = lines.len().min(self.context_lines);
                self.push_context(&lines[..shown]);
                self.push_skip(lines.len() - shown);
            }
            (false, true) => {
                let skipped = lines.len().saturating_sub(self.context_lines);
                self.push_skip(skipped);
                self.push_context(&lines[skipped..]);
            }
            (false, false) => {
                self.old_line += lines.len();
                self.new_line += lines.len();
            }
        }
    }
}

/// `generateDiffString(old, new, contextLines)`: `+N line` / `-N line` / ` N line` rows with line numbers padded to
/// the widest, `contextLines` of context around changes and ` <pad> ...` where context was skipped; and the first
/// changed line (in the new file).
pub fn generate_diff_string(old: &str, new: &str, context_lines: usize) -> (String, Option<usize>) {
    let parts = diff_lines(old, new);
    let max_line = old.split('\n').count().max(new.split('\n').count());
    let mut formatter = DiffFormatter::new(max_line, context_lines);
    let mut last_was_change = false;
    for (index, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }
        if part.added || part.removed {
            formatter.push_change(&raw, part.added);
            last_was_change = true;
            continue;
        }
        let next_is_change = parts.get(index + 1).is_some_and(|next| next.added || next.removed);
        formatter.push_unchanged(&raw, last_was_change, next_is_change);
        last_was_change = false;
    }
    (formatter.output.join("\n"), formatter.first_changed)
}

/// Replays fixtures/diff.json (generate-diff.mjs): jsdiff 8.0.4's `diffLines` and the TypeScript edit tool's
/// `generateDiffString` at context 4 and 2, compared exactly.
#[cfg(test)]
mod tests {
    use super::{diff_lines, generate_diff_string};
    use serde_json::{Value, json};

    fn cases() -> Vec<Value> {
        let fixture: Value = serde_json::from_str(include_str!("../fixtures/diff.json")).unwrap();
        fixture["cases"].as_array().unwrap().clone()
    }

    #[test]
    fn jsrt_diff_lines_matches_jsdiff() {
        for case in cases() {
            let (old, new) = (case["old"].as_str().unwrap(), case["new"].as_str().unwrap());
            let changes: Vec<Value> = diff_lines(old, new)
                .into_iter()
                .map(|c| json!({"value": c.value, "added": c.added, "removed": c.removed}))
                .collect();
            assert_eq!(Value::Array(changes), case["lines"], "{case}");
        }
    }

    #[test]
    fn jsrt_generate_diff_string_matches_the_edit_tool() {
        let cases = cases();
        assert_eq!(cases.len(), 20);
        for case in cases {
            let (old, new) = (case["old"].as_str().unwrap(), case["new"].as_str().unwrap());
            for (context, key) in [(4, "context4"), (2, "context2")] {
                let (diff, first) = generate_diff_string(old, new, context);
                let mut got = json!({ "diff": diff });
                if let Some(first) = first {
                    got["firstChangedLine"] = json!(first);
                }
                assert_eq!(got, case[key], "{key} of {case}");
            }
        }
    }
}
