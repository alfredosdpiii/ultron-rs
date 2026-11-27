//! Result conventions of the model's `bash` (wave 38, from Mirror's tools/bash_tool.rs):
//!
//! - exit code 1 from a read-only comparison command (`grep`, `rg`, `diff`, `cmp`, `test`, ...) reports a result,
//!   not a failure. When the command is a simple pipeline whose last command is one of those, the result carries an
//!   `exit_note` ("No matches found"); runtime.py then shows `[No matches found (exit code 1)]` instead of the
//!   failure line `[exit code 1]`. `exit_code` stays 1 (and `.ok`, a clean exit 0, stays false);
//! - a failed command whose output shows the sandbox refused it carries a `sandbox_note` (`sandbox::refusal`), a
//!   line runtime.py prints after the output, saying what was blocked and what is allowed.

use serde_json::Value;

/// Commands whose exit code 1 reports a result, and the note shown in place of the failure line.
pub const NON_ERROR_EXIT_1: &[(&str, &str)] = &[
    ("grep", "No matches found"),
    ("egrep", "No matches found"),
    ("fgrep", "No matches found"),
    ("rg", "No matches found"),
    ("diff", "Files differ"),
    ("cmp", "Files differ"),
    ("test", "Condition is false"),
    ("[", "Condition is false"),
];

/// The note for `exit_code` of `command`: exit code 1 of a simple pipeline (commands joined by `|` or `|&`, nothing
/// else) whose last command is one of [`NON_ERROR_EXIT_1`]. Anything else (`;`, `&&`, `||`, `&`, a newline, a
/// subshell or command substitution, a `!` negation) may have taken its status from another command: no note.
pub fn exit_note(command: &str, exit_code: Option<i32>) -> Option<&'static str> {
    if exit_code != Some(1) {
        return None;
    }
    let last = pipeline_commands(command)?.pop()?;
    let program = program_of(&last)?;
    let name = program.rsplit('/').next().unwrap_or(&program);
    NON_ERROR_EXIT_1
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, note)| *note)
}

/// Characters that disqualify a command from being a simple pipeline when unquoted.
const DISALLOWED_UNQUOTED: &[char] = &['`', ';', '\n', '(', ')', '{', '}'];

fn is_disallowed_unquoted(character: char, next: Option<&char>) -> bool {
    DISALLOWED_UNQUOTED.contains(&character) || (character == '$' && next == Some(&'('))
}

struct PipelineParser {
    parts: Vec<String>,
    current: String,
    in_single: bool,
    in_double: bool,
}

impl PipelineParser {
    fn new() -> Self {
        Self {
            parts: Vec::new(),
            current: String::new(),
            in_single: false,
            in_double: false,
        }
    }

    fn step(&mut self, character: char, characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<()> {
        if self.in_single {
            self.in_single = character != '\'';
            self.current.push(character);
            return Some(());
        }
        if character == '\\' {
            self.current.push(character);
            self.current.push(characters.next()?);
            return Some(());
        }
        if self.in_double {
            return self.step_double(character, characters);
        }
        self.step_unquoted(character, characters)
    }

    fn step_double(
        &mut self,
        character: char,
        characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    ) -> Option<()> {
        match character {
            '"' => {
                self.in_double = false;
                self.current.push(character);
                Some(())
            }
            '`' => None,
            '$' if characters.peek() == Some(&'(') => None,
            _ => {
                self.current.push(character);
                Some(())
            }
        }
    }

    fn enter_quote(&mut self, quote: char) {
        if quote == '\'' {
            self.in_single = true;
        } else {
            self.in_double = true;
        }
        self.current.push(quote);
    }

    fn step_unquoted(
        &mut self,
        character: char,
        characters: &mut std::iter::Peekable<std::str::Chars<'_>>,
    ) -> Option<()> {
        if is_disallowed_unquoted(character, characters.peek()) {
            return None;
        }
        match character {
            '\'' | '"' => {
                self.enter_quote(character);
                Some(())
            }
            '|' => self.step_pipe(characters),
            '&' => self.step_ampersand(characters),
            '#' => self.step_hash(),
            _ => {
                self.current.push(character);
                Some(())
            }
        }
    }

    fn step_pipe(&mut self, characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<()> {
        match characters.peek() {
            Some('|') => return None,
            Some('&') => {
                characters.next();
            }
            _ => {}
        }
        self.parts.push(std::mem::take(&mut self.current));
        Some(())
    }

    fn step_ampersand(&mut self, characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<()> {
        if self.current.ends_with(['>', '<']) || characters.peek() == Some(&'>') {
            self.current.push('&');
            return Some(());
        }
        None
    }

    fn step_hash(&mut self) -> Option<()> {
        if self.current.is_empty() || self.current.ends_with(char::is_whitespace) {
            return None;
        }
        self.current.push('#');
        Some(())
    }

    fn finish(mut self) -> Option<Vec<String>> {
        if self.in_single || self.in_double {
            return None;
        }
        self.parts.push(self.current);
        self.parts
            .iter()
            .all(|part| !part.trim().is_empty())
            .then_some(self.parts)
    }
}

/// The commands of a simple pipeline, split on `|` outside quotes; None when `command` is anything more.
fn pipeline_commands(command: &str) -> Option<Vec<String>> {
    let mut parser = PipelineParser::new();
    let mut characters = command.trim().chars().peekable();
    while let Some(character) = characters.next() {
        parser.step(character, &mut characters)?;
    }
    parser.finish()
}

/// The program a simple command runs: its first word after `NAME=value` assignments, unquoted (a `!` negation,
/// where exit code 1 means the command succeeded, is a word of its own and no comparison command).
fn program_of(command: &str) -> Option<String> {
    let program = command.split_whitespace().find(|word| !is_assignment(word))?;
    Some(program.trim_matches(|c| c == '\'' || c == '"').to_string())
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Adds the notes to a `bash` result (`output`) or a job snapshot (`text`) of `command`: `exit_note` when the
/// command ended with a non-error exit 1, `sandbox_note` when it failed and `policy` (the sandbox in use, if any)
/// refused it. A result still running, stopped or without an exit code gets neither.
pub fn annotate(result: &mut Value, command: &str, policy: Option<&sandbox::Policy>) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let flag = |key: &str| object.get(key).and_then(Value::as_bool) == Some(true);
    if flag("running") || flag("cancelled") || flag("timed_out") {
        return;
    }
    if object
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status != "completed")
    {
        return;
    }
    let exit_code = object
        .get("exit_code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    if let Some(note) = exit_note(command, exit_code) {
        object.insert("exit_note".into(), note.into());
    }
    let Some(policy) = policy.filter(|_| exit_code.is_some_and(|code| code != 0)) else {
        return;
    };
    let output = ["output", "text"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or_default();
    if let Some(note) = sandbox::refusal::refusal_note(policy, output) {
        object.insert("sandbox_note".into(), note.into());
    }
}
