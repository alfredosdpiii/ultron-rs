//! Chord delta operations over JSON: validation, application, and the wire codec.
//!
//! Port of `packages/chord/src/delta/index.ts` from earendil-works/pi (MIT); the pinned revision is recorded in
//! `fixtures/delta.json`. Errors carry the upstream error name and message so fixtures compare them exactly.
//!
//! Differences from upstream, all of which surface as errors rather than divergent values:
//! - Ops are parsed into [`Op`] before application, so a malformed op is reported even when an earlier op in the
//!   same batch would have failed first.
//! - A trim that splits a UTF-16 surrogate pair fails ([`DeltaError::SplitSurrogate`]): upstream leaves a lone
//!   surrogate, which a Rust `String` cannot hold.
//! - An integer path segment above `usize::MAX` is rejected as unsafe; upstream reports it as unresolvable.
//! - Object key order follows insertion; upstream JavaScript lists integer-like keys first. Key order is not part of
//!   the upstream contract.

mod diff;
mod tracker;

pub use diff::diff_revisions;
pub use tracker::{Draft, Prepared, Tracker, overlap};

use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Path segments that reach the JavaScript prototype chain; rejected everywhere for wire compatibility.
pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Seg {
    Key(String),
    Index(usize),
}

impl Seg {
    fn key(&self) -> Cow<'_, str> {
        match self {
            Seg::Key(key) => Cow::Borrowed(key),
            Seg::Index(index) => Cow::Owned(index.to_string()),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Seg::Key(key) => Value::String(key.clone()),
            Seg::Index(index) => Value::from(*index),
        }
    }
}

pub type Path = Vec<Seg>;

/// A decoded operation. `Set`, `Delete`, `Append` and `Trim` never target the root; `Splice` and `Move` may.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Replace(Value),
    Set(Path, Value),
    Delete(Path),
    Append(Path, String),
    /// Remove this many UTF-16 code units from the front of a string.
    Trim(Path, usize),
    /// Splice an array: index, remove count, inserted items.
    Splice(Path, usize, usize, Vec<Value>),
    /// Reorder an array: `new[i] = old[permutation[i]]`.
    Move(Path, Vec<usize>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaError {
    /// Malformed op or wire tuple (upstream `TypeError`).
    Invalid(String),
    /// The path or path id does not resolve (upstream `PathError`); holds the JSON rendering.
    Path(String),
    /// A reserved, negative, fractional, or out-of-range segment (upstream `UnsafePathError`).
    UnsafePath(String),
    /// A trim would split a UTF-16 surrogate pair; holds the JSON rendering of the path.
    SplitSurrogate(String),
}

impl DeltaError {
    /// The upstream error class name.
    pub fn name(&self) -> &'static str {
        match self {
            DeltaError::Invalid(_) => "TypeError",
            DeltaError::Path(_) => "PathError",
            DeltaError::UnsafePath(_) => "UnsafePathError",
            DeltaError::SplitSurrogate(_) => "SplitSurrogateError",
        }
    }
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::Invalid(message) => f.write_str(message),
            DeltaError::Path(path) => write!(f, "unresolvable path: {path}"),
            DeltaError::UnsafePath(segment) => write!(f, "unsafe path segment: {segment}"),
            DeltaError::SplitSurrogate(path) => write!(f, "trim splits a surrogate pair: {path}"),
        }
    }
}

impl std::error::Error for DeltaError {}

type Result<T> = std::result::Result<T, DeltaError>;

fn invalid<T>(message: &str) -> Result<T> {
    Err(DeltaError::Invalid(message.to_owned()))
}

fn path_json(path: &[Seg]) -> Value {
    Value::Array(path.iter().map(Seg::to_json).collect())
}

fn path_error(path: &[Seg]) -> DeltaError {
    DeltaError::Path(path_json(path).to_string())
}

/// `Number.isInteger(value) && value >= 0`, saturated to `usize`.
fn non_negative_int(value: &Value) -> Option<usize> {
    if let Some(n) = value.as_u64() {
        return Some(usize::try_from(n).unwrap_or(usize::MAX));
    }
    let n = value.as_f64()?;
    (n.is_finite() && n.fract() == 0.0 && n >= 0.0).then_some(n as usize)
}

fn parse_seg(value: &Value) -> Result<Seg> {
    if let Value::String(key) = value {
        if RESERVED_SEGMENTS.contains(&key.as_str()) {
            return Err(DeltaError::UnsafePath(key.clone()));
        }
        return Ok(Seg::Key(key.clone()));
    }
    match non_negative_int(value) {
        Some(index) if index != usize::MAX => Ok(Seg::Index(index)),
        _ => Err(DeltaError::UnsafePath(jsrt::value::string(Some(value)))),
    }
}

fn parse_path(value: &Value, non_empty: bool) -> Result<Path> {
    let Value::Array(segments) = value else {
        return invalid("path is not an array");
    };
    if non_empty && segments.is_empty() {
        return invalid("path is empty");
    }
    segments.iter().map(parse_seg).collect()
}

fn parse_permutation(value: &Value) -> Result<Vec<usize>> {
    let Value::Array(items) = value else {
        return invalid("m permutation is not an array");
    };
    let mut seen = vec![false; items.len()];
    items
        .iter()
        .map(|item| match non_negative_int(item) {
            Some(index) if index < seen.len() && !seen[index] => {
                seen[index] = true;
                Ok(index)
            }
            _ => invalid("m permutation is not a bijection"),
        })
        .collect()
}

fn is_container(value: &Value) -> bool {
    value.is_array() || value.is_object()
}

fn parse_op_tuple(value: &Value) -> Result<(&str, &[Value])> {
    let op = match value {
        Value::Array(op) if !op.is_empty() => op.as_slice(),
        _ => return invalid("op is not a tuple"),
    };
    let verb = match &op[0] {
        Value::String(verb) => verb.as_str(),
        other => {
            return Err(DeltaError::Invalid(format!(
                "unknown op verb: {}",
                jsrt::value::string(Some(other))
            )));
        }
    };
    Ok((verb, op))
}

fn parse_replace_op(op: &[Value]) -> Result<Op> {
    if op.len() == 2 {
        Ok(Op::Replace(op[1].clone()))
    } else {
        invalid("r arity")
    }
}

fn parse_set_op(op: &[Value]) -> Result<Op> {
    if op.len() == 3 {
        Ok(Op::Set(parse_path(&op[1], true)?, op[2].clone()))
    } else {
        invalid("s arity")
    }
}

fn parse_delete_op(op: &[Value]) -> Result<Op> {
    if op.len() == 2 {
        Ok(Op::Delete(parse_path(&op[1], true)?))
    } else {
        invalid("d arity")
    }
}

fn parse_append_op(op: &[Value]) -> Result<Op> {
    match (op.len(), op.get(2)) {
        (3, Some(Value::String(text))) => Ok(Op::Append(parse_path(&op[1], true)?, text.clone())),
        _ => invalid("a shape"),
    }
}

fn parse_trim_op(op: &[Value]) -> Result<Op> {
    match (op.len(), op.get(2).and_then(non_negative_int)) {
        (3, Some(count)) => Ok(Op::Trim(parse_path(&op[1], true)?, count)),
        _ => invalid("t shape"),
    }
}

fn parse_splice_op(op: &[Value]) -> Result<Op> {
    if op.len() != 5 {
        return invalid("p arity");
    }
    let path = parse_path(&op[1], false)?;
    let Some(index) = non_negative_int(&op[2]) else {
        return invalid("p index");
    };
    let Some(remove) = non_negative_int(&op[3]) else {
        return invalid("p remove");
    };
    let Value::Array(items) = &op[4] else {
        return invalid("p items");
    };
    Ok(Op::Splice(path, index, remove, items.clone()))
}

fn parse_move_op(op: &[Value]) -> Result<Op> {
    if op.len() == 3 {
        Ok(Op::Move(parse_path(&op[1], false)?, parse_permutation(&op[2])?))
    } else {
        invalid("m arity")
    }
}

impl Op {
    /// Parse and validate a decoded op tuple (upstream `assertValidOp`).
    pub fn from_json(value: &Value) -> Result<Op> {
        let (verb, op) = parse_op_tuple(value)?;
        match verb {
            "r" => parse_replace_op(op),
            "s" => parse_set_op(op),
            "d" => parse_delete_op(op),
            "a" => parse_append_op(op),
            "t" => parse_trim_op(op),
            "p" => parse_splice_op(op),
            "m" => parse_move_op(op),
            other => Err(DeltaError::Invalid(format!("unknown op verb: {other}"))),
        }
    }

    pub fn to_json(&self) -> Value {
        match self.path() {
            None => Value::Array(vec![Value::from(self.verb()), self.tail().remove(0)]),
            Some(path) => {
                let mut tuple = vec![Value::from(self.verb()), path_json(path)];
                tuple.extend(self.tail());
                Value::Array(tuple)
            }
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Op::Replace(_) => "r",
            Op::Set(..) => "s",
            Op::Delete(_) => "d",
            Op::Append(..) => "a",
            Op::Trim(..) => "t",
            Op::Splice(..) => "p",
            Op::Move(..) => "m",
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Op::Replace(_) => None,
            Op::Set(path, _)
            | Op::Delete(path)
            | Op::Append(path, _)
            | Op::Trim(path, _)
            | Op::Splice(path, ..)
            | Op::Move(path, _) => Some(path),
        }
    }

    /// The tuple elements after the path (for `Replace`, the value).
    fn tail(&self) -> Vec<Value> {
        match self {
            Op::Replace(value) | Op::Set(_, value) => vec![value.clone()],
            Op::Delete(_) => vec![],
            Op::Append(_, text) => vec![Value::from(text.as_str())],
            Op::Trim(_, count) => vec![Value::from(*count)],
            Op::Splice(_, index, remove, items) => {
                vec![Value::from(*index), Value::from(*remove), Value::Array(items.clone())]
            }
            Op::Move(_, permutation) => vec![Value::from(permutation.clone())],
        }
    }
}

/// Apply ops to a value the caller owns exclusively. On error the value is lost: recover from a later `Replace`.
pub fn apply(mut root: Value, ops: Vec<Op>) -> Result<Value> {
    for op in ops {
        apply_op(&mut root, op)?;
    }
    Ok(root)
}

/// Apply one batch without changing `root`.
pub fn apply_immutable(root: &Value, ops: &[Op]) -> Result<Value> {
    apply_immutable_batches(root, [ops])
}

/// Apply ordered batches without changing `root`, returning only the final value.
pub fn apply_immutable_batches<'a>(root: &Value, batches: impl IntoIterator<Item = &'a [Op]>) -> Result<Value> {
    // ponytail: clones the whole root once per call; move to a structurally shared tree if replay cost shows up.
    let mut result = root.clone();
    for ops in batches {
        for op in ops {
            match op {
                Op::Replace(_) => {}
                Op::Splice(path, ..) | Op::Move(path, _) => check_copy_walk(&result, path)?,
                Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Trim(path, _) => {
                    check_copy_walk(&result, &path[..path.len().saturating_sub(1)])?;
                }
            }
            apply_op(&mut result, op.clone())?;
        }
    }
    Ok(result)
}

fn is_array_own_index(items: &[Value], key: &str) -> bool {
    key.parse::<usize>()
        .is_ok_and(|i| i < items.len() && i.to_string() == key)
}

fn copy_walk_step<'a>(node: &'a Value, seg: &Seg) -> Result<Option<&'a Value>> {
    match (node, seg) {
        (Value::Array(items), Seg::Index(index)) => Ok(items.get(*index)),
        (Value::Array(items), Seg::Key(key)) => {
            if key == "length" || is_array_own_index(items, key) {
                return Err(DeltaError::UnsafePath(key.clone()));
            }
            Ok(None)
        }
        (Value::Object(map), seg) => Ok(map.get(seg.key().as_ref())),
        _ => Ok(None),
    }
}

/// Upstream's copy-on-write walk checks the path in a different order than the applier, so a few malformed paths
/// fail with a different error there. Mirror it to report the same error.
fn check_copy_walk(root: &Value, path: &[Seg]) -> Result<()> {
    if !is_container(root) {
        return Err(path_error(path));
    }
    let mut node = root;
    for seg in path {
        match copy_walk_step(node, seg)? {
            Some(child) if is_container(child) => node = child,
            _ => return Err(path_error(path)),
        }
    }
    Ok(())
}

/// Resolve a container at `path` (upstream `resolve`).
fn resolve_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Result<&'a mut Value> {
    let mut node = root;
    for seg in path {
        node = match (node, seg) {
            (Value::Array(items), Seg::Index(index)) => items.get_mut(*index).ok_or_else(|| path_error(path))?,
            (Value::Array(_), Seg::Key(key)) => return Err(DeltaError::UnsafePath(key.clone())),
            (Value::Object(map), seg) => map.get_mut(seg.key().as_ref()).ok_or_else(|| path_error(path))?,
            _ => return Err(path_error(path)),
        };
    }
    if is_container(node) {
        Ok(node)
    } else {
        Err(path_error(path))
    }
}

enum Leaf {
    Set(Value),
    Delete,
    Append(String),
    Trim(usize),
}

fn apply_splice(root: &mut Value, path: &[Seg], index: usize, remove: usize, items: Vec<Value>) -> Result<()> {
    let Value::Array(target) = resolve_mut(root, path)? else {
        return Err(path_error(path));
    };
    let start = index.min(target.len());
    let end = start + remove.min(target.len() - start);
    target.splice(start..end, items);
    Ok(())
}

fn apply_move(root: &mut Value, path: &[Seg], permutation: &[usize]) -> Result<()> {
    let target = match resolve_mut(root, path)? {
        Value::Array(target) if target.len() == permutation.len() => target,
        _ => return Err(path_error(path)),
    };
    let mut previous: Vec<Option<Value>> = target.drain(..).map(Some).collect();
    target.extend(permutation.iter().map(|&i| previous[i].take().unwrap_or(Value::Null)));
    Ok(())
}

fn apply_leaf_slot(slot: Option<&mut Value>, path: &[Seg], leaf: Leaf) -> Result<()> {
    match (slot, leaf) {
        (Some(slot), Leaf::Set(value)) => *slot = value,
        (Some(Value::String(current)), Leaf::Append(text)) => current.push_str(&text),
        (Some(Value::String(current)), Leaf::Trim(count)) => trim_utf16(current, count, path)?,
        _ => return Err(path_error(path)),
    }
    Ok(())
}

fn apply_array_leaf(items: &mut Vec<Value>, key: &Seg, path: &[Seg], leaf: Leaf) -> Result<()> {
    let index = match key {
        Seg::Index(index) => *index,
        Seg::Key(key) => return Err(DeltaError::UnsafePath(key.clone())),
    };
    if index > items.len() {
        return Err(DeltaError::UnsafePath(index.to_string()));
    }
    match leaf {
        Leaf::Set(value) if index == items.len() => {
            items.push(value);
            Ok(())
        }
        Leaf::Delete if index < items.len() => {
            items.remove(index);
            Ok(())
        }
        Leaf::Delete => Err(path_error(path)),
        leaf => apply_leaf_slot(items.get_mut(index), path, leaf),
    }
}

fn apply_object_leaf(map: &mut serde_json::Map<String, Value>, key: &Seg, path: &[Seg], leaf: Leaf) -> Result<()> {
    let key = key.key();
    match leaf {
        Leaf::Set(value) => {
            map.insert(key.into_owned(), value);
            Ok(())
        }
        Leaf::Delete => {
            map.shift_remove(key.as_ref());
            Ok(())
        }
        leaf => apply_leaf_slot(map.get_mut(key.as_ref()), path, leaf),
    }
}

fn apply_leaf(root: &mut Value, path: &[Seg], leaf: Leaf) -> Result<()> {
    let Some((key, parent_path)) = path.split_last() else {
        return Err(path_error(path));
    };
    match resolve_mut(root, parent_path)? {
        Value::Array(items) => apply_array_leaf(items, key, path, leaf),
        Value::Object(map) => apply_object_leaf(map, key, path, leaf),
        _ => Err(path_error(parent_path)),
    }
}

fn apply_op(root: &mut Value, op: Op) -> Result<()> {
    match op {
        Op::Replace(value) => {
            *root = value;
            Ok(())
        }
        Op::Splice(path, index, remove, items) => apply_splice(root, &path, index, remove, items),
        Op::Move(path, permutation) => apply_move(root, &path, &permutation),
        Op::Set(path, value) => apply_leaf(root, &path, Leaf::Set(value)),
        Op::Delete(path) => apply_leaf(root, &path, Leaf::Delete),
        Op::Append(path, text) => apply_leaf(root, &path, Leaf::Append(text)),
        Op::Trim(path, count) => apply_leaf(root, &path, Leaf::Trim(count)),
    }
}

/// Remove `count` UTF-16 code units from the front of `text`.
fn trim_utf16(text: &mut String, count: usize, path: &[Seg]) -> Result<()> {
    let mut units = 0;
    for (offset, ch) in text.char_indices() {
        if units >= count {
            if units > count {
                return Err(DeltaError::SplitSurrogate(path_json(path).to_string()));
            }
            text.drain(..offset);
            return Ok(());
        }
        units += ch.len_utf16();
    }
    if units > count {
        return Err(DeltaError::SplitSurrogate(path_json(path).to_string()));
    }
    text.clear();
    Ok(())
}

/// Interns a path on its second use and omits a path equal to the previous op's.
#[derive(Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, u64>,
    next_id: u64,
}

impl Encoder {
    pub fn encode(&mut self, ops: &[Op]) -> Vec<Value> {
        let mut previous: Option<&Path> = None;
        let mut out = Vec::new();
        for op in ops {
            let Some(path) = op.path() else {
                out.push(op.to_json());
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            };
            let mut tuple = vec![Value::from(op.verb())];
            if previous != Some(path) {
                if let Some(id) = self.ids.get(path) {
                    tuple.push(Value::from(*id));
                } else if self.seen.contains(path) {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(path.clone(), id);
                    out.push(Value::Array(vec![Value::from("#"), Value::from(id), path_json(path)]));
                    tuple.push(Value::from(id));
                } else {
                    self.seen.insert(path.clone());
                    tuple.push(path_json(path));
                }
                previous = Some(path);
            }
            tuple.extend(op.tail());
            out.push(Value::Array(tuple));
        }
        out
    }
}

fn validate_wire_ref(r: &Value) -> Result<()> {
    if r.is_number() {
        return non_negative_int(r)
            .map(|_| ())
            .ok_or_else(|| DeltaError::Invalid("bad path id".to_owned()));
    }
    parse_path(r, false).map(|_| ())
}

fn validate_wire_count(v: &Value, message: &str) -> Result<()> {
    non_negative_int(v).map(|_| ()).map_or_else(|| invalid(message), Ok)
}

fn validate_wire_ref_if(op: &[Value], full_len: usize) -> Result<()> {
    if op.len() == full_len {
        validate_wire_ref(&op[1])?;
    }
    Ok(())
}

fn validate_wire_basic(verb: &str, op: &[Value]) -> Result<()> {
    match (verb, op.len()) {
        ("r", 2) | ("s", 2) | ("d", 1) => Ok(()),
        ("s", 3) | ("d", 2) => validate_wire_ref(&op[1]),
        ("r", _) => invalid("r arity"),
        ("s", _) => invalid("s arity"),
        _ => invalid("d arity"),
    }
}

fn validate_wire_append(op: &[Value]) -> Result<()> {
    if !matches!(op.len(), 2 | 3) {
        return invalid("a arity");
    }
    validate_wire_ref_if(op, 3)?;
    if op[op.len() - 1].is_string() {
        Ok(())
    } else {
        invalid("a value")
    }
}

fn validate_wire_trim(op: &[Value]) -> Result<()> {
    if !matches!(op.len(), 2 | 3) {
        return invalid("t arity");
    }
    validate_wire_ref_if(op, 3)?;
    validate_wire_count(&op[op.len() - 1], "t count")
}

fn validate_wire_splice(op: &[Value]) -> Result<()> {
    if !matches!(op.len(), 4 | 5) {
        return invalid("p arity");
    }
    let rest = &op[op.len() - 3..];
    validate_wire_ref_if(op, 5)?;
    validate_wire_count(&rest[0], "p index")?;
    validate_wire_count(&rest[1], "p remove")?;
    if rest[2].is_array() { Ok(()) } else { invalid("p items") }
}

fn validate_wire_move(op: &[Value]) -> Result<()> {
    if !matches!(op.len(), 2 | 3) {
        return invalid("m arity");
    }
    validate_wire_ref_if(op, 3)?;
    parse_permutation(&op[op.len() - 1]).map(|_| ())
}

fn validate_wire_intern(op: &[Value]) -> Result<()> {
    if op.len() == 3 && non_negative_int(&op[1]).is_some() && op[2].is_array() {
        parse_path(&op[2], false).map(|_| ())
    } else {
        invalid("# shape")
    }
}

/// Validate a wire tuple: ids and short forms are legal here (upstream `assertValidWireOp`).
pub fn validate_wire_op(value: &Value) -> Result<()> {
    let (verb, op) = parse_op_tuple(value)?;
    match verb {
        "r" | "s" | "d" => validate_wire_basic(verb, op),
        "a" => validate_wire_append(op),
        "t" => validate_wire_trim(op),
        "p" => validate_wire_splice(op),
        "m" => validate_wire_move(op),
        "#" => validate_wire_intern(op),
        other => Err(DeltaError::Invalid(format!("unknown op verb: {other}"))),
    }
}

fn is_short_wire_op(verb: &str, len: usize) -> bool {
    match verb {
        "d" => len == 1,
        "p" => len == 4,
        _ => len == 2,
    }
}

/// Validates wire tuples and restores decoded ops. After an error, discard it and recover from a later `Replace`.
#[derive(Default)]
pub struct Decoder {
    paths: HashMap<usize, Path>,
}

impl Decoder {
    fn resolve_wire_path(&self, raw: &Value) -> Result<Path> {
        match non_negative_int(raw) {
            Some(id) => self
                .paths
                .get(&id)
                .cloned()
                .ok_or_else(|| DeltaError::Path(id.to_string())),
            None => parse_path(raw, false),
        }
    }

    fn decode_path_and_tail<'a>(
        &self,
        verb: &str,
        op: &'a [Value],
        previous: &mut Option<Path>,
    ) -> Result<(Path, &'a [Value])> {
        if is_short_wire_op(verb, op.len()) {
            return Ok((previous.clone().ok_or_else(|| path_error(&[]))?, &op[1..]));
        }
        let path = self.resolve_wire_path(&op[1])?;
        *previous = Some(path.clone());
        Ok((path, &op[2..]))
    }

    fn decode_op(&mut self, value: &Value, previous: &mut Option<Path>) -> Result<Option<Op>> {
        validate_wire_op(value)?;
        let op = value.as_array().map_or_default(Vec::as_slice);
        let verb = op[0].as_str().unwrap_or_default();
        if verb == "#" {
            let id = non_negative_int(&op[1]).unwrap_or(usize::MAX);
            self.paths.insert(id, parse_path(&op[2], false)?);
            return Ok(None);
        }
        if verb == "r" {
            self.paths.clear();
            *previous = None;
            return Ok(Some(Op::Replace(op[1].clone())));
        }
        let (path, tail) = self.decode_path_and_tail(verb, op, previous)?;
        if path.is_empty() && verb != "p" && verb != "m" {
            return Err(path_error(&path));
        }
        let mut tuple = vec![op[0].clone(), path_json(&path)];
        tuple.extend_from_slice(tail);
        Ok(Some(Op::from_json(&Value::Array(tuple))?))
    }

    pub fn decode(&mut self, wire: &[Value]) -> Result<Vec<Op>> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::new();
        for value in wire {
            if let Some(op) = self.decode_op(value, &mut previous)? {
                out.push(op);
            }
        }
        Ok(out)
    }
}
