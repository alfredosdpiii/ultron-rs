//! `fork-policy.ts` and `jsonl/fork.ts`: copy a session's tree (every branch) or one branch path into a new
//! format-4 file. Entries keep their ids and sequences; current values are projected (lane state reset to idle,
//! labels of copied entries, the session name); operation, pending, result and usage state are dropped.

use crate::codec::{self, ParsedHeader};
use crate::io;
use crate::legacy_v3::LegacyV3Source;
use crate::ns::{BRANCH_TIP, ENTRY_LABEL, LANE_CONFIG, LANE_STATE, OP_RESULT, SESSION_NAME};
use crate::state::{seq_of, str_of};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Clone, Debug, PartialEq)]
pub enum ForkOptions {
    /// Every entry and branch tip.
    Tree,
    /// The path of `branch` up to `entry_id` (default: its tip), including the entry unless `before`.
    Branch {
        branch: String,
        entry_id: Option<String>,
        any_tree_entry: bool,
        before: bool,
    },
}

/// `ForkCurrentStatePlan`.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Tree,
    Branch {
        branch: String,
        destination_tip: Option<String>,
    },
}

fn walk_branch_ancestors(
    mut current: Option<String>,
    requested: Option<&str>,
    before: bool,
    parent_of: &dyn Fn(&str) -> Option<Option<String>>,
    select: &mut dyn FnMut(&str),
) -> Result<(bool, Option<String>), String> {
    let mut found = requested.is_none();
    let mut destination_tip = None;
    while let Some(id) = current {
        let Some(parent) = parent_of(&id) else {
            return Err(format!("Corrupt source branch: missing parent {id}"));
        };
        if Some(id.as_str()) == requested {
            found = true;
            destination_tip = if before { parent.clone() } else { Some(id.clone()) };
            if !before {
                select(&id);
            }
        } else if found {
            select(&id);
        }
        current = parent;
    }
    Ok((found, destination_tip))
}

/// `selectBranchFork`: the destination tip; selected entries are passed to `select`.
pub fn select_branch_fork(
    options: &ForkOptions,
    tip: Option<Option<String>>,
    parent_of: &dyn Fn(&str) -> Option<Option<String>>,
    select: &mut dyn FnMut(&str),
) -> Result<Plan, String> {
    let ForkOptions::Branch {
        branch,
        entry_id,
        any_tree_entry,
        before,
    } = options
    else {
        return Ok(Plan::Tree);
    };
    let Some(tip) = tip else {
        return Err(format!("Unknown source branch: {branch}"));
    };
    let requested = entry_id.clone().or_else(|| tip.clone());
    let start = if *any_tree_entry && let Some(requested) = &requested {
        if parent_of(requested).is_none() {
            return Err(format!("Unknown fork entry {requested}"));
        }
        Some(requested.clone())
    } else {
        tip
    };
    let (found, destination_tip) = walk_branch_ancestors(start, requested.as_deref(), *before, parent_of, select)?;
    if !found {
        return Err(format!(
            "Fork entry {} is not on source branch {}",
            requested.as_deref().unwrap_or("null"),
            serde_json::to_string(branch).unwrap_or_default()
        ));
    }
    Ok(Plan::Branch {
        branch: branch.clone(),
        destination_tip,
    })
}

fn is_branch_scoped(write: &Value, plan: &Plan) -> bool {
    match plan {
        Plan::Tree => true,
        Plan::Branch { branch, .. } => str_of(write, "key") == branch,
    }
}

fn project_branch_tip(write: &Value, key: &str, plan: &Plan) -> Option<Value> {
    match plan {
        Plan::Tree => Some(write.clone()),
        Plan::Branch {
            branch,
            destination_tip,
        } => (key == branch).then(|| {
            let mut w = write.clone();
            w["value"] = destination_tip.clone().map_or(Value::Null, Value::from);
            w
        }),
    }
}

fn reset_lane_state(write: &Value) -> Value {
    let mut w = write.clone();
    w["value"] = json!({"currentOperationId": null, "lastOperationId": null, "inbox": []});
    w
}

fn is_transient_namespace(namespace: &str) -> bool {
    namespace == OP_RESULT || namespace.starts_with("ultron.op.") || namespace.starts_with("ultron.pending.")
}

fn project_lane_write(namespace: &str, write: &Value, plan: &Plan) -> Option<Value> {
    if !is_branch_scoped(write, plan) {
        return None;
    }
    Some(if namespace == LANE_STATE {
        reset_lane_state(write)
    } else {
        write.clone()
    })
}

/// `projectForkCurrentStateWrite`.
pub fn project_current_state(
    write: &Value,
    plan: &Plan,
    copied: &dyn Fn(&str) -> bool,
) -> Result<Option<Value>, String> {
    let namespace = str_of(write, "namespace");
    let key = str_of(write, "key");
    if is_transient_namespace(namespace) {
        return Ok(None);
    }
    Ok(match namespace {
        SESSION_NAME => Some(write.clone()),
        ENTRY_LABEL => copied(key).then(|| write.clone()),
        BRANCH_TIP => project_branch_tip(write, key, plan),
        LANE_CONFIG | LANE_STATE => project_lane_write(namespace, write, plan),
        _ => matches!(plan, Plan::Tree).then(|| write.clone()),
    })
}

fn physical_key(write: &Value) -> String {
    format!("{}\u{0}{}", str_of(write, "namespace"), str_of(write, "key"))
}

/// `JsonlForkIndex`.
#[derive(Default)]
struct Index {
    current_scalar_seqs: HashMap<String, i64>,
    branch_tips: HashMap<String, Option<String>>,
    first_list_seqs: HashMap<String, i64>,
    parents: HashMap<String, Option<String>>,
    selected: HashSet<String>,
    lane_configs: HashSet<String>,
    lane_states: HashSet<String>,
}

fn update_lane_set(lanes: &mut HashSet<String>, lane: String, set: bool) {
    if set {
        lanes.insert(lane);
    } else {
        lanes.remove(&lane);
    }
}

impl Index {
    fn apply_value(&mut self, write: &Value, key: String, set: bool) {
        if set {
            self.current_scalar_seqs.insert(key, seq_of(write));
        } else {
            self.current_scalar_seqs.remove(&key);
        }
        let lane = str_of(write, "key").to_string();
        match str_of(write, "namespace") {
            BRANCH_TIP if set => {
                self.branch_tips
                    .insert(lane, write["value"].as_str().map(str::to_string));
            }
            BRANCH_TIP => {
                self.branch_tips.remove(&lane);
            }
            LANE_CONFIG => update_lane_set(&mut self.lane_configs, lane, set),
            LANE_STATE => update_lane_set(&mut self.lane_states, lane, set),
            _ => {}
        }
    }

    fn apply(&mut self, writes: &[Value]) {
        for write in writes {
            let key = physical_key(write);
            let set = matches!(str_of(write, "op"), "set" | "append");
            match str_of(write, "kind") {
                "entry" => {
                    self.parents.insert(
                        str_of(write, "id").into(),
                        write.get("parentId").and_then(Value::as_str).map(str::to_string),
                    );
                }
                "value" => self.apply_value(write, key, set),
                "list" if set => {
                    self.first_list_seqs.entry(key).or_insert(seq_of(write));
                }
                "list" => {
                    self.first_list_seqs.remove(&key);
                }
                _ => {}
            }
        }
    }
}

pub enum ForkInput<'a> {
    /// A format-4 file; `next_seq` is the boundary of an open source (later writes are excluded).
    V4 {
        path: &'a Path,
        id: &'a str,
        cwd: &'a str,
        next_seq: Option<i64>,
    },
    LegacyV3(&'a LegacyV3Source),
}

/// A format-4 file's header and its complete transactions.
type Transactions = (Map<String, Value>, Vec<Vec<Value>>);

fn parse_v4_header(path: &Path, first: Option<&str>, id: &str, cwd: &str) -> Result<Map<String, Value>, String> {
    let Some(first) = first.filter(|l| !l.is_empty()) else {
        return Err(format!("Invalid JSONL storage {}: missing header", path.display()));
    };
    let header = match codec::parse_header(first) {
        Ok(ParsedHeader::V4(h)) => h,
        Ok(ParsedHeader::V3(_)) => {
            return Err(format!(
                "Invalid JSONL storage {}: expected format 4 header",
                path.display()
            ));
        }
        Err(_) => return Err(format!("Invalid JSONL storage {}: invalid header", path.display())),
    };
    if codec::header_str(&header, "id") != Some(id) || codec::header_str(&header, "cwd") != Some(cwd) {
        return Err(format!("Session identity does not match header: {id}"));
    }
    let version = codec::header_i64(&header, "storageVersion").unwrap_or(0);
    if version != codec::JSONL_STORAGE_VERSION {
        return Err(format!("Session {id} uses unsupported storage version {version}"));
    }
    Ok(header)
}

/// Complete transactions after the header, stopping at the sequence boundary.
fn v4_transactions(path: &Path, id: &str, cwd: &str, stop_before: Option<i64>) -> Result<Transactions, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to open JSONL fork source {}: {e}", path.display()))?;
    let lines = io::complete_lines(&content);
    let header = parse_v4_header(path, lines.first().copied(), id, cwd)?;
    let mut transactions = Vec::new();
    for line in &lines[1..] {
        let writes = codec::parse_transaction(line)?;
        if let (Some(stop), Some(first), Some(last)) = (stop_before, writes.first(), writes.last()) {
            if seq_of(first) >= stop {
                break;
            }
            if seq_of(last) >= stop {
                return Err(format!("JSONL transaction crosses fork sequence boundary {stop}"));
            }
        }
        transactions.push(writes);
    }
    Ok((header, transactions))
}

fn load_fork_source(input: &ForkInput<'_>, index: &mut Index) -> Result<(Vec<Value>, i64), String> {
    match input {
        ForkInput::LegacyV3(source) => {
            for (id, parent, _) in source.entry_structures() {
                index.parents.insert(id, parent);
            }
            index.apply(&source.values);
            Ok((Vec::new(), source.next_seq))
        }
        ForkInput::V4 {
            path,
            id,
            cwd,
            next_seq,
        } => {
            let (source_header, transactions) = v4_transactions(path, id, cwd, *next_seq)?;
            let mut highest = 0;
            for writes in &transactions {
                index.apply(writes);
                if let Some(last) = writes.last() {
                    highest = seq_of(last);
                }
            }
            let next = next_seq.unwrap_or_else(|| {
                codec::header_i64(&source_header, "nextSeq")
                    .unwrap_or(1)
                    .max(highest + 1)
            });
            Ok((transactions.into_iter().flatten().collect(), next))
        }
    }
}

fn resolve_fork_plan(input: &ForkInput<'_>, options: &ForkOptions, index: &mut Index) -> Result<Plan, String> {
    let mut options = options.clone();
    if let (
        ForkInput::LegacyV3(source),
        ForkOptions::Branch {
            entry_id: Some(entry), ..
        },
    ) = (input, &mut options)
    {
        *entry = source.translate_fork_entry_id(entry)?;
    }
    let ForkOptions::Branch { branch, .. } = &options else {
        return Ok(Plan::Tree);
    };
    let parents = index.parents.clone();
    let mut selected = HashSet::new();
    let plan = select_branch_fork(
        &options,
        index.branch_tips.get(branch).cloned(),
        &|id| parents.get(id).cloned(),
        &mut |id| {
            selected.insert(id.to_string());
        },
    )?;
    index.selected = selected;
    if !(index.lane_configs.contains(branch) && index.lane_states.contains(branch)) {
        return Err(format!(
            "Source branch {} is not a configured AgentLane",
            serde_json::to_string(branch).unwrap_or_default()
        ));
    }
    Ok(plan)
}

fn project_fork_write(
    write: Value,
    index: &Index,
    plan: &Plan,
    copied: &dyn Fn(&str) -> bool,
) -> Result<Option<Value>, String> {
    match str_of(&write, "kind") {
        "entry" => Ok(copied(str_of(&write, "id")).then_some(write)),
        "value" if str_of(&write, "op") == "set" => {
            if index.current_scalar_seqs.get(&physical_key(&write)) == Some(&seq_of(&write)) {
                project_current_state(&write, plan, copied)
            } else {
                Ok(None)
            }
        }
        "list" if str_of(&write, "op") == "append" => {
            let surviving = index
                .first_list_seqs
                .get(&physical_key(&write))
                .is_some_and(|first| seq_of(&write) >= *first);
            if surviving {
                project_current_state(&write, plan, copied)
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// `runJsonlFork`: publish the destination file (header gets the source's `nextSeq`).
pub fn run_fork(
    input: &ForkInput<'_>,
    destination: &Path,
    mut header: Map<String, Value>,
    options: &ForkOptions,
) -> Result<(), String> {
    let mut index = Index::default();
    let (source_writes, next_seq) = load_fork_source(input, &mut index)?;
    let plan = resolve_fork_plan(input, options, &mut index)?;
    let copied = |id: &str| matches!(plan, Plan::Tree) || index.selected.contains(id);
    let writes = match input {
        ForkInput::LegacyV3(source) => source.writes(Some(&copied))?,
        ForkInput::V4 { .. } => source_writes,
    };
    header.insert("nextSeq".into(), jsrt::json::number(next_seq as f64));
    let mut content = format!("{}\n", jsrt::json::stringify(&Value::Object(header)));
    for write in writes {
        if let Some(write) = project_fork_write(write, &index, &plan, &copied)? {
            content.push_str(&codec::serialize_transaction(&[write]));
            content.push('\n');
        }
    }
    io::publish(destination, &content)
}
