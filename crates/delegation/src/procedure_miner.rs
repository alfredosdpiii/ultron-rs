//! Autolith-style automatic procedure mining and earned pre-planning execution.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const MIN_STEPS: usize = 2;
pub const MAX_STEPS: usize = 32;
pub const MAX_RESULT_BYTES: usize = 16 * 1024;
pub const MAX_RECENT_TRACES: usize = 20;
pub const MIN_EXAMPLES: usize = 3;
pub const MAX_EXAMPLES: usize = 8;
pub const CUSTOM_TYPE: &str = "ultron.procedure";

/// One tool invocation captured from a completed turn.
#[derive(Debug, Clone, PartialEq)]
pub struct StepCall {
    pub tool: String,
    pub args: Map<String, Value>,
    pub result: Option<Value>,
    pub ok: bool,
}

/// One completed root turn's prompt, calls, and resource cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Trace {
    pub prompt: String,
    pub steps: Vec<StepCall>,
    pub requests: u32,
    pub tokens: u64,
    pub used_procedure: bool,
}

/// Type inferred for a variable prompt slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    Int,
    Path,
    Word,
}

impl SlotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SlotKind::Int => "int",
            SlotKind::Path => "path",
            SlotKind::Word => "word",
        }
    }

    pub fn parse(raw: &str) -> SlotKind {
        match raw {
            "int" => SlotKind::Int,
            "path" => SlotKind::Path,
            _ => SlotKind::Word,
        }
    }

    pub fn matches(self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        match self {
            SlotKind::Int => token.parse::<i64>().is_ok(),
            SlotKind::Path => token.contains('/') || token.contains('.'),
            SlotKind::Word => true,
        }
    }
}

/// One token in a generalized prompt pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptToken {
    Literal(String),
    Slot { name: String, kind: SlotKind },
}

/// Generalized expression for a step argument.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgExpr {
    Literal(Value),
    Slot(String),
    Template {
        prefix: String,
        slot: String,
        suffix: String,
    },
    StepRef {
        step: usize,
        field: String,
    },
}

/// One generalized step in a mined procedure.
#[derive(Debug, Clone, PartialEq)]
pub struct StepTemplate {
    pub tool: String,
    pub args: BTreeMap<String, ArgExpr>,
}

/// Admission lifecycle of a mined procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcedureState {
    Candidate,
    Admitted,
    Suspended,
    Retired,
}

impl ProcedureState {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcedureState::Candidate => "candidate",
            ProcedureState::Admitted => "admitted",
            ProcedureState::Suspended => "suspended",
            ProcedureState::Retired => "retired",
        }
    }

    pub fn parse(raw: &str) -> ProcedureState {
        match raw {
            "admitted" => ProcedureState::Admitted,
            "suspended" => ProcedureState::Suspended,
            "retired" => ProcedureState::Retired,
            _ => ProcedureState::Candidate,
        }
    }
}

/// A mined procedure persisted per project.
#[derive(Debug, Clone, PartialEq)]
pub struct MinedProcedure {
    pub id: String,
    pub signature: String,
    pub state: ProcedureState,
    pub pattern: Vec<PromptToken>,
    pub steps: Vec<StepTemplate>,
    pub examples: usize,
    pub baseline_requests: f64,
    pub baseline_tokens: f64,
    pub runs: u64,
    pub successes: u64,
    pub failures: u64,
    pub total_requests: u64,
    pub total_tokens: u64,
    pub updated_at: u64,
}

impl MinedProcedure {
    /// Whether this procedure has earned pre-planning privilege based on its recorded telemetry.
    pub fn earns_savings(&self) -> bool {
        if self.runs == 0 {
            return true;
        }
        let avg_requests = (self.total_requests as f64) / (self.runs as f64);
        let avg_tokens = (self.total_tokens as f64) / (self.runs as f64);
        avg_requests < self.baseline_requests && avg_tokens < self.baseline_tokens
    }

    pub fn to_json(&self) -> Value {
        let pattern: Vec<Value> = self
            .pattern
            .iter()
            .map(|tok| match tok {
                PromptToken::Literal(word) => json!({"literal": word}),
                PromptToken::Slot { name, kind } => json!({"slot": name, "kind": kind.as_str()}),
            })
            .collect();
        let steps: Vec<Value> = self
            .steps
            .iter()
            .map(|step| {
                let mut args = Map::new();
                for (k, expr) in &step.args {
                    let encoded = match expr {
                        ArgExpr::Literal(v) => json!({"literal": v}),
                        ArgExpr::Slot(slot) => json!({"slot": slot}),
                        ArgExpr::Template { prefix, slot, suffix } => {
                            json!({"template": {"prefix": prefix, "slot": slot, "suffix": suffix}})
                        }
                        ArgExpr::StepRef { step, field } => {
                            json!({"stepRef": {"step": step, "field": field}})
                        }
                    };
                    args.insert(k.clone(), encoded);
                }
                json!({"tool": step.tool, "args": args})
            })
            .collect();
        json!({
            "id": self.id,
            "signature": self.signature,
            "state": self.state.as_str(),
            "pattern": pattern,
            "steps": steps,
            "examples": self.examples,
            "baselineRequests": self.baseline_requests,
            "baselineTokens": self.baseline_tokens,
            "runs": self.runs,
            "successes": self.successes,
            "failures": self.failures,
            "totalRequests": self.total_requests,
            "totalTokens": self.total_tokens,
            "updatedAt": self.updated_at,
        })
    }

    pub fn from_json(value: &Value) -> Option<MinedProcedure> {
        let id = value["id"].as_str()?.to_string();
        let signature = value["signature"].as_str()?.to_string();
        let state = ProcedureState::parse(value["state"].as_str().unwrap_or("candidate"));
        let mut pattern = Vec::new();
        for tok in value["pattern"].as_array()? {
            if let Some(word) = tok["literal"].as_str() {
                pattern.push(PromptToken::Literal(word.to_string()));
            } else {
                let slot = tok["slot"].as_str()?;
                pattern.push(PromptToken::Slot {
                    name: slot.to_string(),
                    kind: SlotKind::parse(tok["kind"].as_str().unwrap_or("word")),
                });
            }
        }
        let mut steps = Vec::new();
        for step in value["steps"].as_array()? {
            let tool = step["tool"].as_str()?.to_string();
            let mut args = BTreeMap::new();
            for (k, raw) in step["args"].as_object()? {
                let expr = if let Some(lit) = raw.get("literal") {
                    ArgExpr::Literal(lit.clone())
                } else if let Some(slot) = raw["slot"].as_str() {
                    ArgExpr::Slot(slot.to_string())
                } else if let Some(tpl) = raw.get("template").and_then(Value::as_object) {
                    ArgExpr::Template {
                        prefix: tpl.get("prefix").and_then(Value::as_str)?.to_string(),
                        slot: tpl.get("slot").and_then(Value::as_str)?.to_string(),
                        suffix: tpl.get("suffix").and_then(Value::as_str)?.to_string(),
                    }
                } else {
                    let sref = raw.get("stepRef").and_then(Value::as_object)?;
                    ArgExpr::StepRef {
                        step: sref.get("step").and_then(Value::as_u64)? as usize,
                        field: sref.get("field").and_then(Value::as_str)?.to_string(),
                    }
                };
                args.insert(k.clone(), expr);
            }
            steps.push(StepTemplate { tool, args });
        }
        Some(MinedProcedure {
            id,
            signature,
            state,
            pattern,
            steps,
            examples: value["examples"].as_u64().unwrap_or(0) as usize,
            baseline_requests: value["baselineRequests"].as_f64().unwrap_or(0.0),
            baseline_tokens: value["baselineTokens"].as_f64().unwrap_or(0.0),
            runs: value["runs"].as_u64().unwrap_or(0),
            successes: value["successes"].as_u64().unwrap_or(0),
            failures: value["failures"].as_u64().unwrap_or(0),
            total_requests: value["totalRequests"].as_u64().unwrap_or(0),
            total_tokens: value["totalTokens"].as_u64().unwrap_or(0),
            updated_at: value["updatedAt"].as_u64().unwrap_or(0),
        })
    }
}

/// A uniquely selected pre-planning procedure execution plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PreRunPlan {
    pub procedure_id: String,
    pub bindings: BTreeMap<String, String>,
    pub steps: Vec<Value>,
}

fn truncate_value(value: Value) -> Value {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if serialized.len() <= MAX_RESULT_BYTES {
        return value;
    }
    let truncated: String = serialized
        .char_indices()
        .take_while(|(at, _)| *at < MAX_RESULT_BYTES)
        .map(|(_, c)| c)
        .collect();
    Value::String(truncated)
}

fn message_plain_text(message: &Value) -> String {
    if let Some(text) = message["content"].as_str() {
        return text.to_string();
    }
    let mut out = String::new();
    for block in message["content"].as_array().into_iter().flatten() {
        if let Some(text) = block["text"].as_str() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// Extracts completed root turn traces from session journal branch entries.
pub fn extract_traces(entries: &[Value]) -> Vec<Trace> {
    let mut traces = Vec::new();
    let mut current: Option<Trace> = None;
    let mut pending_calls: HashMap<String, usize> = HashMap::new();
    let mut pending_procedure_opening = false;

    for entry in entries {
        if entry["type"] != "message" {
            continue;
        }
        let msg = &entry["message"];
        match msg["role"].as_str() {
            Some("custom") if msg["customType"] == CUSTOM_TYPE => {
                pending_procedure_opening = true;
            }
            Some("user") => {
                if let Some(trace) = current.take()
                    && (MIN_STEPS..=MAX_STEPS).contains(&trace.steps.len())
                    && trace.steps.iter().all(|s| s.ok)
                {
                    traces.push(trace);
                }
                pending_calls.clear();
                current = Some(Trace {
                    prompt: message_plain_text(msg).trim().to_string(),
                    steps: Vec::new(),
                    requests: 0,
                    tokens: 0,
                    used_procedure: std::mem::take(&mut pending_procedure_opening),
                });
            }
            Some("assistant") => {
                let Some(trace) = current.as_mut() else {
                    continue;
                };
                trace.requests += 1;
                trace.tokens += msg["usage"]["totalTokens"].as_u64().unwrap_or(0);
                for block in msg["content"].as_array().into_iter().flatten() {
                    if block["type"] != "toolCall" {
                        continue;
                    }
                    let Some(name) = block["name"].as_str() else {
                        continue;
                    };
                    let args = block["arguments"].as_object().cloned().unwrap_or_default();
                    let idx = trace.steps.len();
                    trace.steps.push(StepCall {
                        tool: name.to_string(),
                        args,
                        result: None,
                        ok: true,
                    });
                    if let Some(id) = block["id"].as_str() {
                        pending_calls.insert(id.to_string(), idx);
                    }
                }
            }
            Some("toolResult") => {
                let Some(trace) = current.as_mut() else {
                    continue;
                };
                let Some(id) = msg["toolCallId"].as_str() else {
                    continue;
                };
                let Some(&idx) = pending_calls.get(id) else {
                    continue;
                };
                let is_err = msg["isError"].as_bool().unwrap_or(false);
                let text = message_plain_text(msg);
                let parsed = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
                if let Some(step) = trace.steps.get_mut(idx) {
                    step.ok = !is_err;
                    step.result = Some(truncate_value(parsed));
                }
            }
            _ => {}
        }
    }
    if let Some(trace) = current
        && (MIN_STEPS..=MAX_STEPS).contains(&trace.steps.len())
        && trace.steps.iter().all(|s| s.ok)
    {
        traces.push(trace);
    }
    let keep_from = traces.len().saturating_sub(MAX_RECENT_TRACES);
    traces.drain(..keep_from);
    traces
}

fn step_signature(steps: &[StepCall]) -> String {
    steps
        .iter()
        .map(|s| {
            let mut keys: Vec<&str> = s.args.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("{}({})", s.tool, keys.join(","))
        })
        .collect::<Vec<_>>()
        .join("->")
}

fn infer_slot_kind(values: &[&str]) -> SlotKind {
    if values.iter().all(|v| v.parse::<i64>().is_ok()) {
        SlotKind::Int
    } else if values.iter().all(|v| v.contains('/') || v.contains('.')) {
        SlotKind::Path
    } else {
        SlotKind::Word
    }
}

type SlotBindings = BTreeMap<String, String>;

fn generalize_prompts(prompts: &[&str]) -> Option<(Vec<PromptToken>, Vec<SlotBindings>)> {
    let tokenized: Vec<Vec<&str>> = prompts
        .iter()
        .map(|p| p.split_whitespace().collect::<Vec<_>>())
        .collect();
    let len = tokenized.first()?.len();
    if len == 0 || tokenized.iter().any(|t| t.len() != len) {
        return None;
    }
    let mut pattern = Vec::with_capacity(len);
    let mut bindings = vec![BTreeMap::new(); prompts.len()];
    let mut slot_index = 0usize;
    let mut literals = 0usize;

    for pos in 0..len {
        let first = tokenized[0][pos];
        if tokenized.iter().all(|tokens| tokens[pos].eq_ignore_ascii_case(first)) {
            pattern.push(PromptToken::Literal(first.to_ascii_lowercase()));
            literals += 1;
        } else {
            let values: Vec<&str> = tokenized.iter().map(|tokens| tokens[pos]).collect();
            let name = format!("arg{slot_index}");
            slot_index += 1;
            let kind = infer_slot_kind(&values);
            for (example_idx, value) in values.into_iter().enumerate() {
                bindings[example_idx].insert(name.clone(), value.to_string());
            }
            pattern.push(PromptToken::Slot { name, kind });
        }
    }
    if literals == 0 {
        return None;
    }
    Some((pattern, bindings))
}

fn generalize_arg(
    step_idx: usize,
    key: &str,
    examples: &[&Trace],
    bindings: &[BTreeMap<String, String>],
) -> Option<ArgExpr> {
    let first = examples[0].steps[step_idx].args.get(key)?;
    if examples
        .iter()
        .all(|ex| ex.steps[step_idx].args.get(key) == Some(first))
    {
        return Some(ArgExpr::Literal(first.clone()));
    }

    if let Some(slot_names) = bindings.first() {
        for slot in slot_names.keys() {
            let exact_match = examples.iter().enumerate().all(|(i, ex)| {
                let Some(val) = ex.steps[step_idx].args.get(key).and_then(Value::as_str) else {
                    return false;
                };
                bindings[i].get(slot).is_some_and(|bound| bound == val)
            });
            if exact_match {
                return Some(ArgExpr::Slot(slot.clone()));
            }
        }
        for slot in slot_names.keys() {
            let Some(first_str) = first.as_str() else {
                continue;
            };
            let Some(first_bound) = bindings[0].get(slot) else {
                continue;
            };
            if first_bound.is_empty() {
                continue;
            }
            let Some(pos) = first_str.find(first_bound.as_str()) else {
                continue;
            };
            let prefix = &first_str[..pos];
            let suffix = &first_str[pos + first_bound.len()..];
            let tpl_match = examples.iter().enumerate().all(|(i, ex)| {
                let Some(val) = ex.steps[step_idx].args.get(key).and_then(Value::as_str) else {
                    return false;
                };
                let Some(bound) = bindings[i].get(slot) else {
                    return false;
                };
                val == format!("{prefix}{bound}{suffix}")
            });
            if tpl_match {
                return Some(ArgExpr::Template {
                    prefix: prefix.to_string(),
                    slot: slot.clone(),
                    suffix: suffix.to_string(),
                });
            }
        }
    }

    for prev in 0..step_idx {
        let Some(first_obj) = examples[0].steps[prev].result.as_ref().and_then(Value::as_object) else {
            continue;
        };
        for field in first_obj.keys() {
            let ref_match = examples.iter().all(|ex| {
                let Some(arg_val) = ex.steps[step_idx].args.get(key) else {
                    return false;
                };
                ex.steps[prev]
                    .result
                    .as_ref()
                    .and_then(Value::as_object)
                    .and_then(|obj| obj.get(field))
                    == Some(arg_val)
            });
            if ref_match {
                return Some(ArgExpr::StepRef {
                    step: prev,
                    field: field.clone(),
                });
            }
        }
    }

    None
}

/// Matches `prompt` against `pattern` requiring exact literal words and valid typed slots.
pub fn match_pattern(pattern: &[PromptToken], prompt: &str) -> Option<BTreeMap<String, String>> {
    let tokens: Vec<&str> = prompt.split_whitespace().collect();
    if tokens.len() != pattern.len() {
        return None;
    }
    let mut bindings = BTreeMap::new();
    for (pat, tok) in pattern.iter().zip(tokens.iter()) {
        match pat {
            PromptToken::Literal(expected) => {
                if !tok.eq_ignore_ascii_case(expected) {
                    return None;
                }
            }
            PromptToken::Slot { name, kind } => {
                if !kind.matches(tok) {
                    return None;
                }
                bindings.insert(name.clone(), (*tok).to_string());
            }
        }
    }
    Some(bindings)
}

fn evaluate_steps_against(steps: &[StepTemplate], bindings: &BTreeMap<String, String>, example: &Trace) -> bool {
    if steps.len() != example.steps.len() {
        return false;
    }
    for (idx, (tpl, actual)) in steps.iter().zip(example.steps.iter()).enumerate() {
        if tpl.tool != actual.tool || tpl.args.len() != actual.args.len() {
            return false;
        }
        for (key, expr) in &tpl.args {
            let expected = match expr {
                ArgExpr::Literal(v) => v.clone(),
                ArgExpr::Slot(slot) => match bindings.get(slot) {
                    Some(v) => Value::String(v.clone()),
                    None => return false,
                },
                ArgExpr::Template { prefix, slot, suffix } => match bindings.get(slot) {
                    Some(v) => Value::String(format!("{prefix}{v}{suffix}")),
                    None => return false,
                },
                ArgExpr::StepRef { step, field } => {
                    if *step >= idx {
                        return false;
                    }
                    match example.steps[*step]
                        .result
                        .as_ref()
                        .and_then(Value::as_object)
                        .and_then(|obj| obj.get(field))
                    {
                        Some(v) => v.clone(),
                        None => return false,
                    }
                }
            };
            if actual.args.get(key) != Some(&expected) {
                return false;
            }
        }
    }
    true
}

fn generalize_group(signature: &str, examples: &[&Trace], now_ms: u64) -> Option<MinedProcedure> {
    let prompts: Vec<&str> = examples.iter().map(|t| t.prompt.as_str()).collect();
    let (pattern, bindings) = generalize_prompts(&prompts)?;
    let step_count = examples[0].steps.len();
    let mut templates = Vec::with_capacity(step_count);
    for step_idx in 0..step_count {
        let tool = examples[0].steps[step_idx].tool.clone();
        let mut args = BTreeMap::new();
        for key in examples[0].steps[step_idx].args.keys() {
            let expr = generalize_arg(step_idx, key, examples, &bindings)?;
            args.insert(key.clone(), expr);
        }
        templates.push(StepTemplate { tool, args });
    }

    for held_out in 0..examples.len() {
        let train: Vec<&Trace> = examples
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != held_out)
            .map(|(_, t)| *t)
            .collect();
        let train_prompts: Vec<&str> = train.iter().map(|t| t.prompt.as_str()).collect();
        let (train_pattern, train_bindings) = generalize_prompts(&train_prompts)?;
        let mut train_steps = Vec::with_capacity(step_count);
        for step_idx in 0..step_count {
            let tool = train[0].steps[step_idx].tool.clone();
            let mut args = BTreeMap::new();
            for key in train[0].steps[step_idx].args.keys() {
                let expr = generalize_arg(step_idx, key, &train, &train_bindings)?;
                args.insert(key.clone(), expr);
            }
            train_steps.push(StepTemplate { tool, args });
        }
        let bound = match_pattern(&train_pattern, &examples[held_out].prompt)?;
        if !evaluate_steps_against(&train_steps, &bound, examples[held_out]) {
            return None;
        }
    }

    let count = examples.len() as f64;
    let baseline_requests = examples.iter().map(|t| f64::from(t.requests)).sum::<f64>() / count;
    let baseline_tokens = examples.iter().map(|t| t.tokens as f64).sum::<f64>() / count;
    let pattern_sig = pattern
        .iter()
        .map(|tok| match tok {
            PromptToken::Literal(w) => w.clone(),
            PromptToken::Slot { kind, .. } => format!("<{}>", kind.as_str()),
        })
        .collect::<Vec<_>>()
        .join(" ");
    let id = format!(
        "proc-{}",
        &jsrt::hash::sha256_hex(format!("{signature}|{pattern_sig}").as_bytes())[..12]
    );

    Some(MinedProcedure {
        id,
        signature: signature.to_string(),
        state: ProcedureState::Candidate,
        pattern,
        steps: templates,
        examples: examples.len(),
        baseline_requests,
        baseline_tokens,
        runs: 0,
        successes: 0,
        failures: 0,
        total_requests: 0,
        total_tokens: 0,
        updated_at: now_ms,
    })
}

/// Mines verified candidate procedures from recent non-procedure traces.
pub fn mine(traces: &[Trace], now_ms: u64) -> Vec<MinedProcedure> {
    let mut groups: BTreeMap<String, Vec<&Trace>> = BTreeMap::new();
    for trace in traces {
        if trace.used_procedure
            || !(MIN_STEPS..=MAX_STEPS).contains(&trace.steps.len())
            || !trace.steps.iter().all(|s| s.ok)
        {
            continue;
        }
        let sig = step_signature(&trace.steps);
        groups.entry(sig).or_default().push(trace);
    }
    let mut mined = Vec::new();
    for (sig, mut group) in groups {
        if group.len() < MIN_EXAMPLES {
            continue;
        }
        let start = group.len().saturating_sub(MAX_EXAMPLES);
        group.drain(..start);
        if let Some(proc) = generalize_group(&sig, &group, now_ms) {
            mined.push(proc);
        }
    }
    mined
}

/// Selects a single `admitted` procedure for pre-planning execution when unambiguously matched and earning savings.
pub fn select_pre_run(prompt: &str, procedures: &[MinedProcedure]) -> Option<PreRunPlan> {
    let mut matches: Vec<(&MinedProcedure, BTreeMap<String, String>)> = Vec::new();
    for proc in procedures {
        if proc.state != ProcedureState::Admitted || !proc.earns_savings() {
            continue;
        }
        if let Some(bindings) = match_pattern(&proc.pattern, prompt) {
            matches.push((proc, bindings));
        }
    }
    if matches.len() != 1 {
        return None;
    }
    let (proc, bindings) = matches.remove(0);
    let steps = proc
        .steps
        .iter()
        .map(|step| {
            let mut resolved = Map::new();
            for (k, expr) in &step.args {
                let value = match expr {
                    ArgExpr::Literal(v) => v.clone(),
                    ArgExpr::Slot(slot) => Value::String(bindings.get(slot).cloned().unwrap_or_default()),
                    ArgExpr::Template { prefix, slot, suffix } => {
                        let bound = bindings.get(slot).map_or_default(String::as_str);
                        Value::String(format!("{prefix}{bound}{suffix}"))
                    }
                    ArgExpr::StepRef { step, field } => Value::String(format!("$step[{step}].{field}")),
                };
                resolved.insert(k.clone(), value);
            }
            json!({"tool": step.tool, "args": resolved})
        })
        .collect();
    Some(PreRunPlan {
        procedure_id: proc.id.clone(),
        bindings,
        steps,
    })
}

/// Formats a selected [`PreRunPlan`] as an opening `ultron.procedure` custom message.
pub fn pre_run_message(plan: &PreRunPlan, timestamp: u64) -> Value {
    let rendered = serde_json::to_string_pretty(&plan.steps).unwrap_or_default();
    let content = format!(
        "[procedure {}] Matched admitted learned procedure (slots: {}). Execute these deterministic steps directly \
         without redundant exploration:\n{rendered}",
        plan.procedure_id,
        serde_json::to_string(&plan.bindings).unwrap_or_default()
    );
    json!({
        "role": "custom",
        "customType": CUSTOM_TYPE,
        "content": content,
        "display": true,
        "details": {
            "procedureId": plan.procedure_id,
            "bindings": plan.bindings,
            "steps": plan.steps,
        },
        "timestamp": timestamp,
    })
}

/// Persistent per-project store of mined procedures under `<agentDir>/procedures/<hash16>.json`.
#[derive(Clone)]
pub struct ProcedureStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl ProcedureStore {
    pub fn new(agent_dir: PathBuf, cwd: PathBuf) -> ProcedureStore {
        let hash = jsrt::hash::sha256_hex(cwd.to_string_lossy().as_bytes());
        ProcedureStore {
            path: agent_dir.join("procedures").join(format!("{}.json", &hash[..16])),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_unlocked(&self) -> Vec<MinedProcedure> {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            return Vec::new();
        };
        doc["procedures"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(MinedProcedure::from_json)
            .collect()
    }

    fn write_unlocked(&self, procedures: &[MinedProcedure]) -> Result<(), String> {
        let encoded: Vec<Value> = procedures.iter().map(MinedProcedure::to_json).collect();
        let body = serde_json::to_string_pretty(&json!({"version": 1, "procedures": encoded}))
            .map_err(|e| format!("serialize procedures: {e}"))?;
        jsrt::fs::atomic_write(&self.path, body).map_err(|e| format!("write {}: {e}", self.path.display()))
    }

    pub fn list(&self) -> Vec<MinedProcedure> {
        let _guard = jsrt::sync::lock(&self.lock);
        self.read_unlocked()
    }

    /// Inserts newly mined candidates or refreshes example counts on existing candidates without changing admitted or
    /// retired states.
    pub fn upsert_mined(&self, mined: Vec<MinedProcedure>, now_ms: u64) -> usize {
        if mined.is_empty() {
            return 0;
        }
        let _guard = jsrt::sync::lock(&self.lock);
        let mut existing = self.read_unlocked();
        let mut added = 0usize;
        for proc in mined {
            if let Some(found) = existing.iter_mut().find(|p| p.id == proc.id) {
                if found.state == ProcedureState::Candidate {
                    found.examples = found.examples.max(proc.examples);
                    found.baseline_requests = proc.baseline_requests;
                    found.baseline_tokens = proc.baseline_tokens;
                    found.updated_at = now_ms;
                }
            } else {
                existing.push(proc);
                added += 1;
            }
        }
        let _ = self.write_unlocked(&existing);
        added
    }

    fn set_state(&self, id: &str, state: ProcedureState, now_ms: u64) -> Result<MinedProcedure, String> {
        let _guard = jsrt::sync::lock(&self.lock);
        let mut existing = self.read_unlocked();
        let Some(found) = existing.iter_mut().find(|p| p.id == id) else {
            return Err(format!("Unknown mined procedure: {id}"));
        };
        found.state = state;
        found.updated_at = now_ms;
        let updated = found.clone();
        self.write_unlocked(&existing)?;
        Ok(updated)
    }

    pub fn admit(&self, id: &str, now_ms: u64) -> Result<MinedProcedure, String> {
        self.set_state(id, ProcedureState::Admitted, now_ms)
    }

    pub fn suspend(&self, id: &str, now_ms: u64) -> Result<MinedProcedure, String> {
        self.set_state(id, ProcedureState::Suspended, now_ms)
    }

    pub fn retire(&self, id: &str, now_ms: u64) -> Result<MinedProcedure, String> {
        self.set_state(id, ProcedureState::Retired, now_ms)
    }

    /// Records telemetry for one execution of `id` and auto-suspends procedures that fail repeatedly.
    pub fn record_run(&self, id: &str, ok: bool, requests: u32, tokens: u64, now_ms: u64) {
        let _guard = jsrt::sync::lock(&self.lock);
        let mut existing = self.read_unlocked();
        let Some(found) = existing.iter_mut().find(|p| p.id == id) else {
            return;
        };
        found.runs += 1;
        if ok {
            found.successes += 1;
        } else {
            found.failures += 1;
        }
        found.total_requests += u64::from(requests);
        found.total_tokens += tokens;
        if found.state == ProcedureState::Admitted && found.failures >= 2 && found.failures * 2 > found.runs {
            found.state = ProcedureState::Suspended;
        }
        found.updated_at = now_ms;
        let _ = self.write_unlocked(&existing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    fn sample_trace(crate_name: &str, path: &str, commit_sha: &str) -> Trace {
        Trace {
            prompt: format!("check crate {crate_name} file {path}"),
            steps: vec![
                StepCall {
                    tool: "read".into(),
                    args: map(json!({"path": path})),
                    result: Some(json!({"sha": commit_sha})),
                    ok: true,
                },
                StepCall {
                    tool: "bash".into(),
                    args: map(json!({
                        "command": format!("cargo test -p {crate_name}"),
                        "rev": commit_sha
                    })),
                    result: Some(json!({"exit": 0})),
                    ok: true,
                },
            ],
            requests: 4,
            tokens: 4_000,
            used_procedure: false,
        }
    }

    #[test]
    fn procedure_miner_generalizes_slots_templates_and_step_refs() {
        let traces = vec![
            sample_trace("ultron-kernel", "crates/kernel/src/lib.rs", "aaa111"),
            sample_trace("ultron-lanes", "crates/lanes/src/lib.rs", "bbb222"),
            sample_trace("ultron-host", "crates/host/src/lib.rs", "ccc333"),
        ];
        let mined = mine(&traces, 1_000);
        assert_eq!(mined.len(), 1);
        let proc = &mined[0];
        assert_eq!(proc.state, ProcedureState::Candidate);
        assert_eq!(proc.examples, 3);
        assert_eq!(
            proc.pattern,
            vec![
                PromptToken::Literal("check".into()),
                PromptToken::Literal("crate".into()),
                PromptToken::Slot {
                    name: "arg0".into(),
                    kind: SlotKind::Word
                },
                PromptToken::Literal("file".into()),
                PromptToken::Slot {
                    name: "arg1".into(),
                    kind: SlotKind::Path
                },
            ]
        );
        assert_eq!(proc.steps[0].args["path"], ArgExpr::Slot("arg1".into()));
        assert_eq!(
            proc.steps[1].args["command"],
            ArgExpr::Template {
                prefix: "cargo test -p ".into(),
                slot: "arg0".into(),
                suffix: String::new(),
            }
        );
        assert_eq!(
            proc.steps[1].args["rev"],
            ArgExpr::StepRef {
                step: 0,
                field: "sha".into(),
            }
        );
    }

    #[test]
    fn procedure_miner_rejects_unreproducible_or_procedure_traces() {
        let mut bad = ["a", "b", "c"]
            .iter()
            .enumerate()
            .map(|(i, k)| sample_trace(k, &format!("src/{k}.rs"), &format!("{i}{i}{i}")))
            .collect::<Vec<_>>();
        bad[2].steps[1]
            .args
            .insert("command".into(), json!("unrelated command"));
        assert!(
            mine(&bad, 1_000).is_empty(),
            "held-out check must reject non-generalizable arg"
        );

        let mut replayed = ["d", "e", "f"]
            .iter()
            .enumerate()
            .map(|(i, k)| sample_trace(k, &format!("src/{k}.rs"), &format!("r{i}")))
            .collect::<Vec<_>>();
        replayed[0].used_procedure = true;
        assert!(
            mine(&replayed, 1_000).is_empty(),
            "procedure turns must never be re-mined"
        );
    }

    #[test]
    fn procedure_store_lifecycle_and_earned_pre_run_selection() {
        let dir = tempfile::tempdir().unwrap();
        let store = ProcedureStore::new(dir.path().join("agent"), dir.path().join("repo"));
        let traces = vec![
            sample_trace("ultron-kernel", "crates/kernel/src/lib.rs", "aaa"),
            sample_trace("ultron-lanes", "crates/lanes/src/lib.rs", "bbb"),
            sample_trace("ultron-host", "crates/host/src/lib.rs", "ccc"),
        ];
        let mined = mine(&traces, 100);
        let id = mined[0].id.clone();
        assert_eq!(store.upsert_mined(mined, 100), 1);

        assert!(
            select_pre_run("check crate ultron-ai file crates/ai/src/lib.rs", &store.list()).is_none(),
            "candidate must not pre-run before admission"
        );
        store.admit(&id, 200).unwrap();

        let plan = select_pre_run("check crate ultron-ai file crates/ai/src/lib.rs", &store.list()).unwrap();
        assert_eq!(plan.procedure_id, id);
        assert_eq!(plan.steps[1]["args"]["command"], "cargo test -p ultron-ai");
        assert_eq!(plan.steps[1]["args"]["rev"], "$step[0].sha");

        assert!(
            select_pre_run("check crate ultron-ai file not_a_path", &store.list()).is_none(),
            "typed Path slot must reject plain word"
        );

        store.record_run(&id, true, 5, 5_000, 300);
        assert!(
            select_pre_run("check crate ultron-ai file crates/ai/src/lib.rs", &store.list()).is_none(),
            "must lose pre-run privilege when average cost exceeds baseline"
        );

        store.record_run(&id, true, 1, 1_000, 400);
        assert!(
            select_pre_run("check crate ultron-ai file crates/ai/src/lib.rs", &store.list()).is_some(),
            "earns pre-run back once average requests (3.0 < 4.0) and tokens (3000 < 4000) beat baseline"
        );

        store.retire(&id, 500).unwrap();
        assert!(select_pre_run("check crate ultron-ai file crates/ai/src/lib.rs", &store.list()).is_none());
    }
}
