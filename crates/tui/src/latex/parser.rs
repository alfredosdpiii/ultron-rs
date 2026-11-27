use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::utils::visible_width;
use jsrt::string;

use super::PROTECTED_SPACE;
use super::layout::{
    LAYOUT_MARKER_START, NAMED_OPERATOR_END, NAMED_OPERATOR_START, Node, is_simple, marker, normalize_output,
    trailing_marker,
};
use super::tables::{
    accent, blackboard, is_display_limit_symbol, is_font_switch, is_ignored, is_limit_operator, is_named_operator,
    is_negative_spacing, is_plain_wrapper, is_relation, is_size, is_spacing, negated, subscript, superscript, symbol,
};

const NEGATIVE_SPACE: &str = "\0";

static NUMERIC_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{N}.]+$").expect("regex"));
static LEADING_SPEC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[\t\n\x0B\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*\{[^}]*\}",
    )
    .expect("regex")
});
static ROW_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\\(?:\[[^\]\n]*\])?").expect("regex"));
static OPERATOR_MODIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\\(limits|nolimits)(?![A-Za-z])").expect("regex"));

const LITERAL_COMMANDS: &[(&str, &str)] = &[
    ("\\", "\n"),
    ("{", "{"),
    ("}", "}"),
    ("$", "$"),
    ("%", "%"),
    ("#", "#"),
    ("_", "_"),
    ("&", "&"),
    ("|", "‖"),
    ("mod", " mod "),
    ("bmod", " mod "),
];

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn replace_characters(value: &str, map: fn(char) -> Option<&'static str>) -> Option<String> {
    value.chars().map(map).collect()
}

/// `value.trim().replace(/\s*([=+-])\s*/g, "$1")`.
fn normalize_script_value(value: &str) -> String {
    let chars: Vec<char> = string::trim(value).chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let mut j = i;
        while j < chars.len() && string::is_space(chars[j]) {
            j += 1;
        }
        if j < chars.len() && matches!(chars[j], '=' | '+' | '-') {
            out.push(chars[j]);
            i = j + 1;
            while i < chars.len() && string::is_space(chars[i]) {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sub,
    Sup,
}

fn format_unicode_script(value: &str, kind: Kind) -> Option<String> {
    let map = if kind == Kind::Sub { subscript } else { superscript };
    replace_characters(&normalize_script_value(value), map)
}

fn format_script(value: &str, kind: Kind) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, kind) {
        return unicode;
    }
    let prefix = if kind == Kind::Sub { '_' } else { '^' };
    if char_count(&value) == 1
        || (kind == Kind::Sub && !value.is_empty() && value.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let (n, d) = (string::trim(numerator), string::trim(denominator));
    let simple_d = NUMERIC_VALUE.is_match(d).unwrap_or(false) || char_count(d) == 1;
    let n = if is_simple(n) { n.to_string() } else { format!("({n})") };
    let d = if simple_d { d.to_string() } else { format!("({d})") };
    format!("{n}/{d}")
}

fn format_root(value: &str, symbol: &str) -> String {
    let value = string::trim(value);
    if is_simple(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

fn trim_end_in_place(s: &mut String) {
    let n = s.trim_end_matches(string::is_space).len();
    s.truncate(n);
}

fn split_rows(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in ROW_SPLIT.find_iter(body).flatten() {
        out.push(body[last..m.start()].to_string());
        last = m.end();
    }
    out.push(body[last..].to_string());
    out
}

fn strip_leading_spec(body: &str) -> String {
    match LEADING_SPEC.find(body).ok().flatten() {
        Some(m) => body[m.end()..].to_string(),
        None => body.to_string(),
    }
}

/// `value.replace(/,\s*$/, "")`.
fn strip_trailing_comma(value: &str) -> &str {
    value
        .trim_end_matches(string::is_space)
        .strip_suffix(',')
        .unwrap_or(value)
}

/// `/^(?:if|when|for|otherwise)\b/i`.
fn starts_with_condition_word(s: &str) -> bool {
    ["if", "when", "for", "otherwise"].iter().any(|w| {
        s.get(..w.len()).is_some_and(|p| p.eq_ignore_ascii_case(w))
            && !s[w.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn script_disallows_layout(value: Option<&str>) -> bool {
    let Some(v) = value else { return false };
    v.contains('/')
        || (!v.contains(LAYOUT_MARKER_START)
            && char_count(v) > 1
            && !v.chars().any(|c| c.is_ascii_uppercase() || c == '*' || c == '∗'))
}

fn format_inline_scripts(
    order: &[Kind],
    sub: Option<&str>,
    sup: Option<&str>,
    sub_u: Option<&str>,
    sup_u: Option<&str>,
) -> String {
    let mut out = String::new();
    for &k in order {
        match k {
            Kind::Sub => match sub_u {
                Some(u) => out.push_str(u),
                None => out.push_str(&format_script(sub.unwrap_or(""), k)),
            },
            Kind::Sup => match sup_u {
                Some(u) => out.push_str(u),
                None => out.push_str(&format_script(sup.unwrap_or(""), k)),
            },
        }
    }
    out
}

fn format_inline_operator(
    operator: &str,
    bracket: bool,
    spaced: bool,
    lower: Option<&str>,
    upper: Option<&str>,
) -> String {
    let mut rendered = operator.to_string();
    if let Some(l) = lower {
        rendered += &if bracket {
            format!("[{l}]")
        } else {
            format_script(l, Kind::Sub)
        };
    }
    if let Some(u) = upper {
        rendered += &format_script(u, Kind::Sup);
    }
    if spaced { format!(" {rendered} ") } else { rendered }
}

enum CommandToken {
    Newline,
    Name(String),
    Eof,
}

pub(super) struct Parser<'a> {
    source: Vec<char>,
    nodes: &'a mut Vec<Node>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
    script_depth: usize,
}

impl<'a> Parser<'a> {
    pub(super) fn new(source: &str, nodes: &'a mut Vec<Node>, display: bool) -> Self {
        Parser {
            source: source.chars().collect(),
            nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    fn at(&self, i: usize) -> Option<char> {
        self.source.get(i).copied()
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.source[a..b].iter().collect()
    }

    fn push_node(&mut self, node: Node) -> String {
        self.nodes.push(node);
        marker(self.nodes.len() - 1)
    }

    fn skip(&mut self, pred: fn(char) -> bool) {
        while self.at(self.position).is_some_and(pred) {
            self.position += 1;
        }
    }

    pub(super) fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn handle_sequence_backslash(&mut self, result: &mut String) {
        let command = self.parse_command();
        if command == NEGATIVE_SPACE {
            trim_end_in_place(result);
            if result.ends_with(NAMED_OPERATOR_END) {
                result.pop();
            }
        } else {
            *result += &command;
        }
    }

    fn handle_sequence_script(&mut self, marker_ch: char, result: &mut String) {
        self.position += 1;
        trim_end_in_place(result);
        let script = self.parse_scripts(marker_ch);
        if result.ends_with(NAMED_OPERATOR_END) {
            result.pop();
            *result += &script;
            result.push(NAMED_OPERATOR_END);
        } else {
            *result += &script;
        }
    }

    fn handle_sequence_default(&mut self, c: char, result: &mut String) {
        if c == '.'
            && let Some(Node::Matrix { lines, .. }) = trailing_marker(result).and_then(|i| self.nodes.get_mut(i))
        {
            if let Some(last) = lines.last_mut() {
                last.push('.');
            }
            self.position += 1;
            return;
        }
        result.push(c);
        self.position += 1;
    }

    fn parse_sequence_char(&mut self, c: char, result: &mut String) {
        match c {
            '{' => {
                self.position += 1;
                *result += &self.parse_sequence(Some('}'));
            }
            '\\' => self.handle_sequence_backslash(result),
            '^' | '_' => self.handle_sequence_script(c, result),
            c if string::is_space(c) => {
                self.skip(string::is_space);
                result.push(' ');
            }
            '=' | '<' | '>' => {
                trim_end_in_place(result);
                result.push(' ');
                result.push(c);
                result.push(' ');
                self.position += 1;
            }
            '&' => self.position += 1,
            '~' => {
                self.position += 1;
                result.push(' ');
            }
            _ => self.handle_sequence_default(c, result),
        }
    }

    fn parse_sequence(&mut self, end: Option<char>) -> String {
        let mut result = String::new();
        while let Some(c) = self.at(self.position) {
            if end == Some(c) {
                self.position += 1;
                return result;
            }
            if c == '}' {
                self.supported = false;
                return result;
            }
            self.parse_sequence_char(c, &mut result);
        }
        if end.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_single_script(
        &mut self,
        marker_ch: char,
        sub: &mut Option<String>,
        sup: &mut Option<String>,
        order: &mut Vec<Kind>,
    ) {
        self.script_depth += 1;
        let value = self.parse_required_argument(false);
        self.script_depth -= 1;
        if marker_ch == '_' {
            *sub = Some(value);
            order.push(Kind::Sub);
        } else {
            *sup = Some(value);
            order.push(Kind::Sup);
        }
    }

    fn peek_paired_script(&mut self, initial: char) -> Option<char> {
        let mut next = self.position;
        while self.at(next).is_some_and(string::is_space) {
            next += 1;
        }
        let m = self.at(next)?;
        if (m == '^' || m == '_') && m != initial {
            self.position = next + 1;
            Some(m)
        } else {
            None
        }
    }

    fn parse_scripts(&mut self, initial: char) -> String {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order = Vec::new();
        self.parse_single_script(initial, &mut sub, &mut sup, &mut order);
        if let Some(m) = self.peek_paired_script(initial) {
            self.parse_single_script(m, &mut sub, &mut sup, &mut order);
        }
        let sub_u = sub.as_deref().and_then(|v| format_unicode_script(v, Kind::Sub));
        let sup_u = sup.as_deref().and_then(|v| format_unicode_script(v, Kind::Sup));
        let can_use_layout = !script_disallows_layout(sub.as_deref()) && !script_disallows_layout(sup.as_deref());
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0 || (sub.is_some() && sub_u.is_none()) || (sup.is_some() && sup_u.is_none()));
        if !needs_layout {
            return format_inline_scripts(
                &order,
                sub.as_deref(),
                sup.as_deref(),
                sub_u.as_deref(),
                sup_u.as_deref(),
            );
        }
        self.push_node(Node::Script {
            lower: sub.as_deref().map(normalize_output),
            upper: sup.as_deref().map(normalize_output),
        })
    }

    fn read_command_name(&mut self) -> CommandToken {
        self.position += 1;
        let Some(first) = self.at(self.position) else {
            self.supported = false;
            return CommandToken::Eof;
        };
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.at(self.position) == Some('\n') {
                self.position += 1;
            }
            return CommandToken::Newline;
        }
        let name = if first.is_ascii_alphabetic() {
            let start = self.position;
            self.skip(|c| c.is_ascii_alphabetic());
            self.slice(start, self.position)
        } else {
            self.position += 1;
            first.to_string()
        };
        CommandToken::Name(name)
    }

    fn parse_trivial_command(&mut self, command: &str) -> Option<String> {
        if let Some(&(_, lit)) = LITERAL_COMMANDS.iter().find(|&&(k, _)| k == command) {
            return Some(lit.into());
        }
        if is_spacing(command) {
            return Some(" ".into());
        }
        if is_negative_spacing(command) {
            return Some(NEGATIVE_SPACE.into());
        }
        if is_font_switch(command) {
            self.skip(string::is_space);
            return Some(String::new());
        }
        if is_ignored(command) || is_size(command) {
            return Some(String::new());
        }
        None
    }

    fn parse_not_command(&mut self) -> String {
        let value = self.parse_required_argument(false);
        let value = string::trim(&value);
        if let Some(n) = negated(value) {
            return format!(" {n} ");
        }
        let mut chars = value.chars();
        let Some(head) = chars.next() else {
            self.supported = false;
            return String::new();
        };
        format!(" {head}\u{338}{} ", chars.as_str())
    }

    fn parse_symbol_or_named_op(&mut self, command: &str) -> Option<String> {
        if is_limit_operator(command) {
            return Some(self.parse_operator(command, true, true, true));
        }
        if let Some(sym) = symbol(command) {
            if is_display_limit_symbol(command) {
                return Some(self.parse_operator(sym, false, true, false));
            }
            return Some(if command == "cdot" || command == "times" || is_relation(command) {
                format!(" {sym} ")
            } else {
                sym.into()
            });
        }
        if is_named_operator(command) {
            return Some(format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}"));
        }
        None
    }

    fn parse_frac_command(&mut self, command: &str) -> String {
        let stack = self.display && self.stack_fractions && command != "tfrac";
        let numerator = self.parse_required_argument(!stack);
        let denominator = self.parse_required_argument(!stack);
        if stack {
            return self.push_node(Node::Fraction {
                numerator: normalize_output(&numerator),
                denominator: normalize_output(&denominator),
            });
        }
        format_fraction(&numerator, &denominator)
    }

    fn parse_sqrt_command(&mut self) -> String {
        let degree = self.parse_optional_argument();
        let value = self.parse_required_argument(true);
        match degree.as_deref().map(string::trim) {
            None | Some("2") => format_root(&value, "√"),
            Some("3") => format_root(&value, "∛"),
            Some("4") => format_root(&value, "∜"),
            Some(d) => format!("{}{}", format_script(d, Kind::Sup), format_root(&value, "√")),
        }
    }

    fn parse_math_arg_command(&mut self, command: &str) -> Option<String> {
        match command {
            "left" | "middle" | "right" => {
                if self.at(self.position) == Some('.') {
                    self.position += 1;
                }
                Some(String::new())
            }
            "frac" | "dfrac" | "tfrac" => Some(self.parse_frac_command(command)),
            "sqrt" => Some(self.parse_sqrt_command()),
            "boxed" | "fbox" => Some(format!("[{}]", string::trim(&self.parse_required_argument(true)))),
            "binom" | "dbinom" | "tbinom" => {
                let a = self.parse_required_argument(true);
                let b = self.parse_required_argument(true);
                Some(format!("({a} choose {b})"))
            }
            _ => None,
        }
    }

    fn parse_styled_or_script_command(&mut self, command: &str) -> Option<String> {
        if let Some(acc) = accent(command) {
            let value = self.parse_required_argument(true);
            return Some(if char_count(&value) == 1 {
                format!("{value}{acc}")
            } else {
                format!("{command}({value})")
            });
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            return Some(
                value
                    .chars()
                    .map(|c| blackboard(c).map_or_else(|| c.to_string(), str::to_string))
                    .collect(),
            );
        }
        if command == "operatorname" {
            let starred = self.at(self.position) == Some('*');
            if starred {
                self.position += 1;
            }
            let arg = self.parse_required_argument(true);
            let operator = string::trim(&normalize_output(&arg)).to_string();
            return Some(self.parse_operator(&operator, true, starred, true));
        }
        self.parse_misc_command(command)
    }

    fn parse_over_under_set(&mut self, command: &str) -> String {
        let script = self.parse_required_argument(true);
        let value = self.parse_required_argument(true);
        let kind = if command == "underset" { Kind::Sub } else { Kind::Sup };
        format!("{}{}", string::trim(&value), format_script(&script, kind))
    }

    fn parse_misc_command(&mut self, command: &str) -> Option<String> {
        if command == "pmod" || command == "pod" {
            let value = self.parse_required_argument(true);
            let value = string::trim(&value);
            return Some(if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            });
        }
        if matches!(command, "overset" | "stackrel" | "underset") {
            return Some(self.parse_over_under_set(command));
        }
        if is_plain_wrapper(command) {
            let value = self.parse_required_argument(true);
            return Some(if command.starts_with("text") || command == "mbox" {
                value
            } else {
                string::trim(&value).into()
            });
        }
        None
    }

    fn parse_command(&mut self) -> String {
        let command = match self.read_command_name() {
            CommandToken::Eof => return String::new(),
            CommandToken::Newline => return " ".into(),
            CommandToken::Name(name) => name,
        };
        let command = command.as_str();
        if let Some(res) = self.parse_trivial_command(command) {
            return res;
        }
        if command == "not" {
            return self.parse_not_command();
        }
        if let Some(res) = self
            .parse_symbol_or_named_op(command)
            .or_else(|| self.parse_math_arg_command(command))
            .or_else(|| self.parse_styled_or_script_command(command))
        {
            return res;
        }
        if command == "begin" {
            return self.parse_environment();
        }
        self.supported = false;
        if command == "end" {
            String::new()
        } else {
            format!("\\{command}")
        }
    }

    fn parse_operator_modifier(&mut self, default: bool) -> bool {
        let mut modifier_position = self.position;
        while self.at(modifier_position).is_some_and(|c| c == ' ' || c == '\t') {
            modifier_position += 1;
        }
        let rest = self.slice(modifier_position, self.source.len());
        if let Ok(Some(m)) = OPERATOR_MODIFIER.captures(&rest) {
            let all = m.get(0).map_or("", |x| x.as_str());
            self.position = modifier_position + char_count(all);
            return m.get(1).is_some_and(|x| x.as_str() == "limits");
        }
        default
    }

    fn parse_operator_scripts(&mut self) -> (Option<String>, Option<String>) {
        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while self.at(script_position).is_some_and(|c| c == ' ' || c == '\t') {
                script_position += 1;
            }
            let kind = self.at(script_position);
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = script_position + 1;
            let arg = self.parse_required_argument(false);
            let value = normalize_output(&arg).replace(' ', "");
            let slot = if kind == Some('_') { &mut lower } else { &mut upper };
            if slot.is_some() {
                self.supported = false;
            }
            *slot = Some(value);
        }
        (lower, upper)
    }

    fn parse_operator(&mut self, operator: &str, bracket: bool, display_limits: bool, spaced: bool) -> String {
        let use_display_limits = self.parse_operator_modifier(display_limits);
        let (lower, upper) = self.parse_operator_scripts();
        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            return self.push_node(Node::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
        }
        format_inline_operator(operator, bracket, spaced, lower.as_deref(), upper.as_deref())
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        self.skip(string::is_space);
        match self.at(self.position) {
            None => {
                self.supported = false;
                String::new()
            }
            Some('{') => {
                self.position += 1;
                self.parse_sequence(Some('}'))
            }
            Some('\\') => self.parse_command(),
            Some(c) => {
                self.position += 1;
                c.to_string()
            }
        }
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        self.skip(|c| c == ' ' || c == '\t');
        if self.at(self.position) != Some('[') {
            return None;
        }
        let Some(end) = (self.position + 1..self.source.len()).find(|&i| self.source[i] == ']') else {
            self.supported = false;
            return None;
        };
        let value = self.slice(self.position + 1, end);
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn read_raw_group(&mut self) -> Option<String> {
        self.skip(|c| c == ' ' || c == '\t');
        if self.at(self.position) != Some('{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1;
        while let Some(c) = self.at(self.position) {
            if c == '\\' {
                self.position += 2;
                continue;
            }
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value = self.slice(start, self.position);
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn read_environment_body(&mut self) -> Option<(String, String)> {
        let environment = self.read_raw_group().filter(|e| !e.is_empty())?;
        let end_marker: Vec<char> = format!("\\end{{{environment}}}").chars().collect();
        let from = self.position.min(self.source.len());
        let Some(end) = (from..self.source.len()).find(|&i| self.source[i..].starts_with(&end_marker)) else {
            self.supported = false;
            return None;
        };
        let body = self.slice(from, end);
        self.position = end + end_marker.len();
        Some((environment, body))
    }

    fn render_aligned_env(&mut self, env: &str, body: &str) -> String {
        let aligned_at = matches!(env, "alignedat" | "alignat" | "alignat*");
        let body = if aligned_at {
            strip_leading_spec(body)
        } else {
            body.to_string()
        };
        let mut rows = Vec::new();
        for row in split_rows(&body) {
            let cells: Vec<&str> = row.split('&').collect();
            let source = if aligned_at {
                cells.chunks(2).map(<[&str]>::concat).collect::<Vec<_>>().join(" ")
            } else {
                cells.concat()
            };
            let rendered = string::trim(&self.render_nested(&source, true)).to_string();
            if !rendered.is_empty() {
                rows.push(rendered);
            }
        }
        rows.join("\n")
    }

    fn parse_environment(&mut self) -> String {
        let Some((environment, body)) = self.read_environment_body() else {
            return String::new();
        };
        let env = environment.as_str();
        if matches!(env, "equation" | "equation*" | "displaymath") {
            return string::trim(&self.render_nested(&body, true)).to_string();
        }
        if matches!(
            env,
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            return self.render_aligned_env(env, &body);
        }
        if env == "cases" || env == "cases*" {
            return self.render_cases(&body);
        }
        if matches!(
            env,
            "array" | "matrix" | "smallmatrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix"
        ) {
            let body = if env == "array" {
                strip_leading_spec(&body)
            } else {
                body
            };
            return self.render_matrix(env, &body);
        }
        self.supported = false;
        body
    }

    fn table(&mut self, body: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        for row in split_rows(body) {
            let cells: Vec<String> = row
                .split('&')
                .map(|cell| string::trim(&self.render_nested(cell, false)).to_string())
                .collect();
            if cells.iter().any(|c| !c.is_empty()) {
                rows.push(cells);
            }
        }
        rows
    }

    fn render_cases(&mut self, body: &str) -> String {
        let rows = self.table(body);
        let value_width = rows
            .iter()
            .map(|r| visible_width(strip_trailing_comma(&r[0])))
            .max()
            .unwrap_or(0);
        let contents: Vec<String> = rows
            .iter()
            .map(|row| {
                let value = strip_trailing_comma(&row[0]);
                let condition = row.get(1).map_or("", String::as_str);
                if condition.is_empty() {
                    return value.to_string();
                }
                let prefix = if starts_with_condition_word(condition) {
                    " "
                } else {
                    " if "
                };
                let pad: String = std::iter::repeat_n(PROTECTED_SPACE, value_width - visible_width(value)).collect();
                format!("{value}{pad}{prefix}{condition}")
            })
            .collect();
        if contents.len() <= 1 {
            return contents.first().map_or(String::new(), |c| format!("⎧ {c}"));
        }
        let middle = contents.len() / 2;
        let mut visual: Vec<Option<String>> = contents.into_iter().map(Some).collect();
        if visual.len().is_multiple_of(2) {
            visual.insert(middle, None);
        }
        let n = visual.len();
        let lines = visual
            .into_iter()
            .enumerate()
            .map(|(i, content)| {
                let delimiter = if i == 0 {
                    "⎧"
                } else if i == n - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                content.map_or_else(|| delimiter.to_string(), |c| format!("{delimiter} {c}"))
            })
            .collect();
        self.push_node(Node::Matrix {
            lines,
            baseline: middle,
        })
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix = self.table(body);
        let columns = matrix.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                matrix
                    .iter()
                    .map(|r| r.get(c).map_or(0, |x| visible_width(x)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..columns)
                    .map(|c| {
                        let cell = row.get(c).map_or("", String::as_str);
                        let pad: String =
                            std::iter::repeat_n(PROTECTED_SPACE, widths[c].saturating_sub(visible_width(cell)))
                                .collect();
                        format!("{cell}{pad}")
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();
        let lines: Vec<String> = if matches!(environment, "array" | "matrix" | "smallmatrix") {
            rows
        } else {
            let d: [&str; 6] = match environment {
                "pmatrix" => ["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"],
                "bmatrix" => ["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"],
                "Bmatrix" => ["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"],
                "vmatrix" => ["│"; 6],
                _ => ["║"; 6],
            };
            let n = rows.len();
            rows.iter()
                .enumerate()
                .map(|(i, row)| {
                    let (l, r) = if i == 0 {
                        (d[0], d[1])
                    } else if i == n - 1 {
                        (d[4], d[5])
                    } else {
                        (d[2], d[3])
                    };
                    format!("{l} {row} {r}")
                })
                .collect()
        };
        if lines.len() <= 1 {
            return lines.into_iter().next().unwrap_or_default();
        }
        self.push_node(Node::Matrix { lines, baseline: 0 })
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let display = self.display && stack_fractions;
        match Parser::new(source, self.nodes, display).render() {
            Some(r) => r,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}
