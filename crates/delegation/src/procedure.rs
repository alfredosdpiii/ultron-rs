//! Skill procedures (Rust-only): a SKILL.md whose frontmatter has `procedure:` is a list of segments the host walks
//! and checks, so a skill's steps are enforced rather than read. `skills.run(name, input)` in the REPL starts one
//! ([`crate::skill_runs`]); this module is the format, its parse and the pure views over run records.
//!
//! ```yaml
//! procedure:
//!   inputs:                      # what skills.run(name, input={...}) must give; {{name}} inserts one
//!     test_command: the one command that runs only the new test
//!     test_files: globs of the files the test lives in
//!   segments:
//!     - name: failing-test       # [a-z][a-z0-9-]*, unique
//!       goal: Write the smallest test that shows the bug.
//!       setup: "notes = []"      # Python run in the REPL when the segment begins (optional)
//!       requires: {files: [Cargo.toml], budget_usd: 0.5}   # checked when it begins (optional)
//!       writes: ["{{test_files}}"]   # the only files it may change (optional: no boundary)
//!       check: {run: "{{test_command}}", expect: fail}      # or a command string (exit 0)
//!       max_iters: 3             # failed checks before the run fails (default 3)
//!     - name: right-reason
//!       goal: Say why the test fails; the judge reads it beside the failing output.
//!       judge: {kind: skill.tdd, questions: {intended: {type: yesno, instructions: ...}}, pass: {intended: true}}
//!     - name: review
//!       goal: ...
//!       agent: {brief: ..., model: anthropic/claude-haiku-5-5, checks: [{run: cargo test -q}]}
//! ```
//!
//! A segment has one step: a `check` command (a contract `run` check: `expect` "exit 0", `{"exit": N}`, "fail" or
//! `{"stdout_contains": s}`), a `judge` decision (`judge.decide` with the model's statement and the last check's
//! output as state; it passes when every `pass` answer matches) or an `agent` (a subagent on `model`, the step's
//! model tier, whose `checks` the host runs when it ends, as a spawn contract's).

use crate::spawn_contract::HostCheck;
use kernel_apis::memory_records::Glob;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const MAX_SEGMENTS: usize = 16;
pub const DEFAULT_MAX_ITERS: u32 = 3;
pub const MAX_ITERS: u32 = 10;
const MAX_GOAL: usize = 4_000;
const MAX_SETUP: usize = 20_000;

/// What a segment does to pass.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// A command the host runs (always a [`HostCheck::Run`]).
    Check(HostCheck),
    Judge {
        kind: String,
        questions: Map<String, Value>,
        pass: Map<String, Value>,
    },
    Agent {
        brief: String,
        model: String,
        checks: Vec<HostCheck>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub name: String,
    pub goal: String,
    pub setup: Option<String>,
    pub requires_files: Vec<String>,
    pub budget_usd: Option<f64>,
    /// Globs of the files the segment may change; `None`: any.
    pub writes: Option<Vec<String>>,
    pub step: Step,
    pub max_iters: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Procedure {
    /// Input name -> description.
    pub inputs: BTreeMap<String, String>,
    pub segments: Vec<Segment>,
}

fn only(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("{what}: unknown field {key}")),
        None => Ok(()),
    }
}

fn text(value: Option<&Value>, what: &str, limit: usize) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() && text.chars().count() <= limit => Ok(text.to_string()),
        _ => Err(format!(
            "{what} must be a nonempty string of at most {limit} characters"
        )),
    }
}

fn list(value: Option<&Value>, what: &str) -> Result<Vec<String>, String> {
    match value.and_then(Value::as_array) {
        Some(items) if items.len() <= 100 => items
            .iter()
            .map(|item| match item.as_str().map(str::trim) {
                Some(text) if !text.is_empty() => Ok(text.to_string()),
                _ => Err(format!("{what} must be a list of nonempty strings")),
            })
            .collect(),
        _ => Err(format!("{what} must be a list of at most 100 strings")),
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 40
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The placeholders (`{{name}}`) in `text`.
fn placeholders(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push(rest[start + 2..start + 2 + end].trim());
        rest = &rest[start + 2 + end + 2..];
    }
    out
}

/// `raw` with every `{{name}}` replaced by `input[name]`: a list item that is only a placeholder of a list input
/// becomes its items; elsewhere strings, numbers and booleans are inserted as text.
fn bind(raw: &Value, input: &Map<String, Value>) -> Result<Value, String> {
    let fill = |text: &str| -> Result<String, String> {
        let mut out = text.to_string();
        for name in placeholders(text) {
            let value = match input.get(name) {
                Some(Value::String(text)) => text.clone(),
                Some(value @ (Value::Number(_) | Value::Bool(_))) => value.to_string(),
                Some(_) => return Err(format!("input {name} must be a string here (it fills {text:?})")),
                None => return Err(format!("missing input {name}")),
            };
            out = out.replace(&format!("{{{{{name}}}}}"), &value);
        }
        Ok(out)
    };
    Ok(match raw {
        Value::String(text) => Value::String(fill(text)?),
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                let whole = item
                    .as_str()
                    .map(str::trim)
                    .filter(|t| t.starts_with("{{") && t.ends_with("}}") && placeholders(t).len() == 1)
                    .and_then(|t| input.get(placeholders(t)[0]));
                match whole {
                    Some(Value::Array(values)) => out.extend(values.iter().cloned()),
                    _ => out.push(bind(item, input)?),
                }
            }
            Value::Array(out)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| Ok((key.clone(), bind(value, input)?)))
                .collect::<Result<_, String>>()?,
        ),
        other => other.clone(),
    })
}

fn parse_check(value: &Value, at: &str) -> Result<HostCheck, String> {
    let written = match value {
        Value::String(command) => json!({ "run": command }),
        Value::Object(map) if map.contains_key("run") => value.clone(),
        _ => return Err(format!("{at}.check must be a command or {{run, expect, timeout_ms}}")),
    };
    HostCheck::parse(&written, 0).map_err(|error| format!("{at}.check: {error}"))
}

fn parse_segment(value: &Value, index: usize) -> Result<Segment, String> {
    let at = format!("procedure.segments[{index}]");
    let map = value.as_object().ok_or_else(|| format!("{at} must be an object"))?;
    only(
        map,
        &[
            "name",
            "goal",
            "setup",
            "requires",
            "writes",
            "check",
            "judge",
            "agent",
            "max_iters",
        ],
        &at,
    )?;
    let name = text(map.get("name"), &format!("{at}.name"), 40)?;
    if !valid_name(&name) {
        return Err(format!("{at}.name must match [a-z][a-z0-9-]* (at most 40 characters)"));
    }
    let at = format!("procedure segment {name}");
    let goal = text(map.get("goal"), &format!("{at}.goal"), MAX_GOAL)?;
    let setup = match map.get("setup") {
        None | Some(Value::Null) => None,
        value => Some(text(value, &format!("{at}.setup"), MAX_SETUP)?),
    };
    let (mut requires_files, mut budget_usd) = (Vec::new(), None);
    if let Some(requires) = map.get("requires") {
        let requires = requires
            .as_object()
            .ok_or_else(|| format!("{at}.requires must be {{files, budget_usd}}"))?;
        only(requires, &["files", "budget_usd"], &format!("{at}.requires"))?;
        if requires.contains_key("files") {
            requires_files = list(requires.get("files"), &format!("{at}.requires.files"))?;
        }
        if let Some(budget) = requires.get("budget_usd") {
            budget_usd = Some(
                budget
                    .as_f64()
                    .filter(|usd| usd.is_finite() && *usd >= 0.0)
                    .ok_or_else(|| format!("{at}.requires.budget_usd must be a number of dollars"))?,
            );
        }
    }
    let writes = match map.get("writes") {
        None | Some(Value::Null) => None,
        value => {
            let globs = list(value, &format!("{at}.writes"))?;
            if let Some(bad) = globs.iter().find(|glob| Glob::new(glob).is_err()) {
                return Err(format!("{at}.writes has an invalid glob: {bad}"));
            }
            Some(globs)
        }
    };
    let steps: Vec<&str> = ["check", "judge", "agent"]
        .into_iter()
        .filter(|key| map.contains_key(*key))
        .collect();
    let step = match steps.as_slice() {
        ["check"] => Step::Check(parse_check(&map["check"], &at)?),
        ["judge"] => {
            let judge = map["judge"]
                .as_object()
                .ok_or_else(|| format!("{at}.judge must be {{kind, questions, pass}}"))?;
            only(judge, &["kind", "questions", "pass"], &format!("{at}.judge"))?;
            let kind = match judge.get("kind") {
                None => "skill".to_string(),
                value => text(value, &format!("{at}.judge.kind"), 64)?,
            };
            let questions = judge
                .get("questions")
                .and_then(Value::as_object)
                .filter(|questions| !questions.is_empty())
                .cloned()
                .ok_or_else(|| format!("{at}.judge.questions must be a nonempty object of questions"))?;
            let pass = judge
                .get("pass")
                .and_then(Value::as_object)
                .filter(|pass| !pass.is_empty())
                .cloned()
                .ok_or_else(|| format!("{at}.judge.pass must name the answers that pass ({{question: answer}})"))?;
            if let Some(unknown) = pass.keys().find(|key| !questions.contains_key(*key)) {
                return Err(format!("{at}.judge.pass names {unknown}, which is not a question"));
            }
            Step::Judge { kind, questions, pass }
        }
        ["agent"] => {
            let agent = map["agent"]
                .as_object()
                .ok_or_else(|| format!("{at}.agent must be {{brief, model, checks}}"))?;
            only(agent, &["brief", "model", "checks"], &format!("{at}.agent"))?;
            let checks = match agent.get("checks") {
                None => Vec::new(),
                Some(Value::Array(items)) => items
                    .iter()
                    .enumerate()
                    .map(|(i, check)| HostCheck::parse(check, i).map_err(|e| format!("{at}.agent: {e}")))
                    .collect::<Result<_, _>>()?,
                Some(_) => return Err(format!("{at}.agent.checks must be a list of contract checks")),
            };
            Step::Agent {
                brief: text(agent.get("brief"), &format!("{at}.agent.brief"), MAX_GOAL)?,
                model: text(agent.get("model"), &format!("{at}.agent.model (its model tier)"), 200)?,
                checks,
            }
        }
        _ => return Err(format!("{at} needs exactly one of check, judge or agent")),
    };
    let max_iters = match map.get("max_iters") {
        None => DEFAULT_MAX_ITERS,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_ITERS as u64).contains(n))
            .ok_or_else(|| format!("{at}.max_iters must be an integer from 1 to {MAX_ITERS}"))?
            as u32,
    };
    Ok(Segment {
        name,
        goal,
        setup,
        requires_files,
        budget_usd,
        writes,
        step,
        max_iters,
    })
}

/// The `procedure` frontmatter value (as JSON), its placeholders filled from `input`.
pub fn parse(raw: &Value, input: &Value) -> Result<Procedure, String> {
    let map = raw.as_object().ok_or("procedure must be {inputs, segments}")?;
    only(map, &["inputs", "segments"], "procedure")?;
    let mut inputs = BTreeMap::new();
    if let Some(declared) = map.get("inputs") {
        for (name, description) in declared
            .as_object()
            .ok_or("procedure.inputs must be {name: description}")?
        {
            inputs.insert(name.clone(), description.as_str().unwrap_or_default().to_string());
        }
    }
    let given = match input {
        Value::Null => Map::new(),
        Value::Object(map) => map.clone(),
        _ => return Err("input must be an object of the procedure's inputs".into()),
    };
    if let Some(unknown) = given.keys().find(|key| !inputs.contains_key(*key)) {
        return Err(format!(
            "unknown input {unknown} (the procedure takes: {})",
            names(&inputs)
        ));
    }
    let missing: Vec<&String> = inputs.keys().filter(|name| !given.contains_key(*name)).collect();
    if !missing.is_empty() {
        let wanted: Vec<String> = missing
            .iter()
            .map(|name| format!("{name} ({})", inputs[*name]))
            .collect();
        return Err(format!("missing input: {}", wanted.join(", ")));
    }
    let segments = map
        .get("segments")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty() && items.len() <= MAX_SEGMENTS)
        .ok_or_else(|| format!("procedure.segments must be a list of 1 to {MAX_SEGMENTS} segments"))?;
    let undeclared = placeholders_in(&Value::Array(segments.clone()))
        .into_iter()
        .find(|name| !inputs.contains_key(name));
    if let Some(name) = undeclared {
        return Err(format!(
            "procedure uses {{{{{name}}}}}, which procedure.inputs does not declare"
        ));
    }
    let bound = bind(&Value::Array(segments.clone()), &given)?;
    let segments: Vec<Segment> = bound
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, segment)| parse_segment(segment, index))
        .collect::<Result<_, _>>()?;
    let mut seen = std::collections::HashSet::new();
    if let Some(twice) = segments.iter().find(|segment| !seen.insert(segment.name.as_str())) {
        return Err(format!("procedure has two segments named {}", twice.name));
    }
    Ok(Procedure { inputs, segments })
}

fn names(inputs: &BTreeMap<String, String>) -> String {
    match inputs.is_empty() {
        true => "none".into(),
        false => inputs.keys().cloned().collect::<Vec<_>>().join(", "),
    }
}

fn placeholders_in(value: &Value) -> Vec<String> {
    match value {
        Value::String(text) => placeholders(text).into_iter().map(str::to_string).collect(),
        Value::Array(items) => items.iter().flat_map(placeholders_in).collect(),
        Value::Object(map) => map.values().flat_map(placeholders_in).collect(),
        _ => Vec::new(),
    }
}

/// The `procedure` of a SKILL.md, as JSON; `None` when it has none.
pub fn raw_of(content: &str) -> Result<Option<Value>, String> {
    let parsed = resources::frontmatter::parse(content)?;
    let raw = resources::frontmatter::to_json(&parsed.frontmatter);
    Ok(raw.get("procedure").filter(|value| !value.is_null()).cloned())
}

/// Check a procedure's shape without input: every declared input stands in as its own name (a list input as a
/// one-item list).
pub fn validate(raw: &Value) -> Result<Procedure, String> {
    let stand_in: Map<String, Value> = raw["inputs"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, _)| (name.clone(), Value::String(name.clone())))
        .collect();
    parse(raw, &Value::Object(stand_in))
}

impl Segment {
    pub fn kind(&self) -> &'static str {
        match self.step {
            Step::Check(_) => "check",
            Step::Judge { .. } => "judge",
            Step::Agent { .. } => "agent",
        }
    }

    /// What the model is shown of the segment.
    pub fn brief(&self, index: usize, total: usize) -> Value {
        let mut out = json!({
            "index": index,
            "of": total,
            "name": self.name,
            "goal": self.goal,
            "kind": self.kind(),
            "writes": self.writes,
            "max_iters": self.max_iters,
            "setup": self.setup,
        });
        match &self.step {
            Step::Check(check) => out["check"] = check.describe(),
            Step::Judge { questions, pass, .. } => {
                out["questions"] = Value::Object(questions.clone());
                out["pass"] = Value::Object(pass.clone());
            }
            Step::Agent { model, checks, .. } => {
                out["model"] = json!(model);
                out["checks"] = checks.iter().map(HostCheck::describe).collect();
            }
        }
        if !self.requires_files.is_empty() || self.budget_usd.is_some() {
            out["requires"] = json!({"files": self.requires_files, "budget_usd": self.budget_usd});
        }
        out
    }

    /// Whether `path` (relative, `/`) is inside the segment's boundary.
    pub fn may_write(&self, path: &str) -> bool {
        match &self.writes {
            None => true,
            Some(globs) => globs
                .iter()
                .filter_map(|glob| Glob::new(glob).ok())
                .any(|glob| glob.matches(path)),
        }
    }
}

impl Procedure {
    /// The run's final check: the last `check` segment's command, re-run by the host when the run ends.
    pub fn final_check(&self) -> Option<(usize, &HostCheck)> {
        self.segments
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, segment)| match &segment.step {
                Step::Check(check) => Some((index, check)),
                _ => None,
            })
    }
}

/// `skills.coverage(name)` over the session's run records of the skill: per outcome, and per segment of `current`
/// (the catalog's procedure) how often runs reached, passed and failed it and with how many tries. Runs of other
/// versions are counted, not mixed in.
pub fn coverage(name: &str, version: &str, current: Option<&Procedure>, runs: &[Value]) -> Value {
    let mine: Vec<&Value> = runs.iter().filter(|run| run["name"] == name).collect();
    let (same, other): (Vec<&Value>, Vec<&Value>) = mine.iter().partition(|run| run["version"] == version);
    let mut outcomes = Map::new();
    for run in &same {
        let outcome = run["outcome"].as_str().unwrap_or("running").to_string();
        let count = outcomes.get(&outcome).and_then(Value::as_u64).unwrap_or(0) + 1;
        outcomes.insert(outcome, count.into());
    }
    let segments: Vec<Value> = current
        .map(|procedure| {
            procedure
                .segments
                .iter()
                .map(|segment| {
                    let results: Vec<&Value> = same
                        .iter()
                        .filter_map(|run| run["segments"].as_array())
                        .flatten()
                        .filter(|result| result["name"] == segment.name.as_str())
                        .collect();
                    let passed = results.iter().filter(|r| r["outcome"] == "passed").count();
                    let tries: u64 = results.iter().filter_map(|r| r["tries"].as_u64()).sum();
                    json!({
                        "name": segment.name,
                        "kind": segment.kind(),
                        "reached": results.len(),
                        "passed": passed,
                        "failed": results.iter().filter(|r| r["outcome"] == "failed").count(),
                        "mean_tries": if results.is_empty() { Value::Null } else { json!(tries as f64 / results.len() as f64) },
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let never_passed: Vec<Value> = segments
        .iter()
        .filter(|segment| segment["passed"] == 0)
        .map(|segment| segment["name"].clone())
        .collect();
    json!({
        "name": name,
        "version": version,
        "runs": same.len(),
        "outcomes": outcomes,
        "segments": segments,
        "never_passed": never_passed,
        "other_versions": other.len(),
    })
}

/// Skill proposals (dreaming's or the model's): a SKILL.md with a procedure, waiting in
/// `<agent dir>/skill-proposals/<id>/` until the user publishes it (`ultron skills approve <id>`) into
/// `<agent dir>/skills/<name>/SKILL.md`, or rejects it. Nothing here publishes on its own.
pub mod proposals {
    use super::*;

    const RECORD: &str = "proposal.json";
    const SKILL: &str = "SKILL.md";
    const MAX_CONTENT: usize = 100_000;

    fn valid_skill_name(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !name.starts_with('-')
            && !name.ends_with('-')
    }

    /// A proposed SKILL.md: frontmatter `name` = `name`, a description, and a procedure that parses.
    pub fn check_content(name: &str, content: &str) -> Result<Procedure, String> {
        if !valid_skill_name(name) {
            return Err("name must be lowercase letters, digits and hyphens (at most 64)".into());
        }
        if content.len() > MAX_CONTENT {
            return Err(format!("content must be at most {MAX_CONTENT} bytes"));
        }
        let parsed = resources::frontmatter::parse(content)?;
        if resources::frontmatter::string_field(&parsed.frontmatter, "name") != Some(name) {
            return Err(format!("the frontmatter's name must be {name}"));
        }
        if resources::frontmatter::string_field(&parsed.frontmatter, "description").is_none_or(|d| d.trim().is_empty())
        {
            return Err("the frontmatter needs a description".into());
        }
        let raw = raw_of(content)?.ok_or("a proposed skill needs a procedure: in its frontmatter")?;
        validate(&raw)
    }

    /// Evidence: 1 to 20 `{ref, quote?}`, each `ref` a session span (`session://<id>#<from>..<to>`) or a skill run
    /// (`skill-run:<id>`): where the procedure worked.
    fn check_evidence(evidence: &Value) -> Result<Vec<Value>, String> {
        let items = evidence
            .as_array()
            .filter(|items| !items.is_empty() && items.len() <= 20)
            .ok_or("evidence must be a list of 1 to 20 {ref, quote}")?;
        items
            .iter()
            .map(|item| {
                let reference = item["ref"].as_str().unwrap_or_default();
                if !(reference.starts_with("session://") || reference.starts_with("skill-run:")) {
                    return Err(format!(
                        "evidence ref must be a session span (session://<id>#<from>..<to>) or skill-run:<id>, got {reference:?}"
                    ));
                }
                let quote = item.get("quote").and_then(Value::as_str);
                if quote.is_some_and(|q| q.len() > 1_000) {
                    return Err("an evidence quote must be at most 1000 bytes".into());
                }
                Ok(json!({"ref": reference, "quote": quote}))
            })
            .collect()
    }

    fn read(dir: &Path, id: &str) -> Result<(PathBuf, Value), String> {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(format!("Unknown skill proposal: {id}"));
        }
        let path = dir.join(id);
        let text = std::fs::read_to_string(path.join(RECORD)).map_err(|_| format!("Unknown skill proposal: {id}"))?;
        let record = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((path, record))
    }

    fn write(path: &Path, record: &Value) -> Result<(), String> {
        let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
        std::fs::write(path.join(RECORD), text + "\n").map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Keep a proposal. `session` is the proposing session (provenance with the evidence). A proposal with the same
    /// content still waiting is not kept twice.
    pub fn propose(
        dir: &Path,
        name: &str,
        content: &str,
        evidence: &Value,
        session: Option<&str>,
        at: i64,
    ) -> Result<Value, String> {
        let procedure = check_content(name, content)?;
        let evidence = check_evidence(evidence)?;
        let digest = jsrt::hash::sha256_hex(content.as_bytes());
        if let Some(same) = list(dir)?
            .into_iter()
            .find(|record| record["state"] == "proposed" && record["sha256"] == digest.as_str())
        {
            return Err(format!(
                "Duplicate skill proposal: {} is already waiting",
                same["id"].as_str().unwrap_or("")
            ));
        }
        let id = jsrt::random::uuid_v4();
        let path = dir.join(&id);
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        std::fs::write(path.join(SKILL), content).map_err(|e| format!("{}: {e}", path.display()))?;
        let record = json!({
            "id": id,
            "name": name,
            "state": "proposed",
            "at": at,
            "session": session,
            "evidence": evidence,
            "segments": procedure.segments.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
            "sha256": digest,
        });
        write(&path, &record)?;
        Ok(record)
    }

    /// Every proposal, oldest first.
    pub fn list(dir: &Path) -> Result<Vec<Value>, String> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(Vec::new());
        };
        let mut out: Vec<Value> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| read(dir, &entry.file_name().to_string_lossy()).ok())
            .map(|(_, record)| record)
            .collect();
        out.sort_by_key(|record| record["at"].as_i64().unwrap_or(0));
        Ok(out)
    }

    /// A proposal's record and SKILL.md text.
    pub fn show(dir: &Path, id: &str) -> Result<(Value, String), String> {
        let (path, record) = read(dir, id)?;
        let content = std::fs::read_to_string(path.join(SKILL)).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok((record, content))
    }

    /// The user's approval of the SKILL.md whose sha256 they were shown (`shown`): check it again and publish it as
    /// `<skills_dir>/<name>/SKILL.md`, never over an existing skill. Content that changed since it was shown is
    /// refused.
    pub fn approve(dir: &Path, id: &str, skills_dir: &Path, at: i64, shown: &str) -> Result<Value, String> {
        let (path, mut record) = read(dir, id)?;
        if record["state"] != "proposed" {
            return Err(format!(
                "Skill proposal {id} is {}",
                record["state"].as_str().unwrap_or("?")
            ));
        }
        let name = record["name"].as_str().unwrap_or_default().to_string();
        let content = std::fs::read_to_string(path.join(SKILL)).map_err(|e| format!("{}: {e}", path.display()))?;
        let digest = jsrt::hash::sha256_hex(content.as_bytes());
        if digest != shown {
            return Err(format!(
                "Skill proposal {id} changed after it was shown (sha256 now {}); run approve again to see it",
                &digest[..12]
            ));
        }
        check_content(&name, &content)?;
        let target = skills_dir.join(&name);
        if target.join(SKILL).exists() {
            return Err(format!(
                "{} exists; remove it or rename the proposal first",
                target.join(SKILL).display()
            ));
        }
        std::fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?;
        std::fs::write(target.join(SKILL), &content).map_err(|e| format!("{}: {e}", target.display()))?;
        record["state"] = json!("published");
        record["published_at"] = json!(at);
        record["path"] = json!(target.join(SKILL).display().to_string());
        record["sha256"] = json!(digest);
        write(&path, &record)?;
        Ok(record)
    }

    pub fn reject(dir: &Path, id: &str, reason: Option<&str>, at: i64) -> Result<Value, String> {
        let (path, mut record) = read(dir, id)?;
        if record["state"] != "proposed" {
            return Err(format!(
                "Skill proposal {id} is {}",
                record["state"].as_str().unwrap_or("?")
            ));
        }
        record["state"] = json!("rejected");
        record["rejected_at"] = json!(at);
        record["reason"] = json!(reason);
        write(&path, &record)?;
        Ok(record)
    }
}
