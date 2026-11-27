//! `instances.*` (port of `ultron/instances.ts`): a completed RLM task can be retained so later invocations run on
//! the same lane, continuing its conversation and declared `state`, while each invocation gets its own task record.
//! An open instance pins its lane's kernel (holder `instance:<id>`); closing unpins it, and a restarted owner re-pins
//! every open instance. Pinning is best effort: retain reports `pinned: false` when the pool has no capacity.

use crate::store::read_versioned;
use crate::util::Clock;
use crate::{ApiModule, Request, TaskHost};
use kernel::store::DocumentStore;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const TERMINAL: &[&str] = &["completed", "failed", "cancelled", "interrupted"];

fn text(value: Option<&Value>, name: &str) -> Result<String, String> {
    match value.and_then(Value::as_str) {
        Some(text) if !jsrt::string::trim(text).is_empty() => Ok(text.to_string()),
        _ => Err(format!("{name} must be a nonempty string")),
    }
}

fn holder(instance: &Value) -> String {
    format!("instance:{}", instance["id"].as_str().unwrap_or(""))
}

/// Root may inspect and drive every instance; a task only its own.
fn assert_owner(instance: &Value, caller: Option<&str>) -> Result<(), String> {
    match caller {
        Some(caller) if instance["owner"] != caller => Err("Instance belongs to another task".into()),
        _ => Ok(()),
    }
}

fn find_index(document: &Value, id: Option<&Value>) -> Result<usize, String> {
    let id = text(id, "id")?;
    document["instances"]
        .as_array()
        .and_then(|items| items.iter().position(|item| item["id"] == id.as_str()))
        .ok_or_else(|| "Unknown instance".to_string())
}

struct State {
    document: Option<Value>,
    pinned: HashSet<String>,
}

pub struct InstancesModule {
    store: Arc<dyn DocumentStore>,
    now: Option<Clock>,
    state: Mutex<State>,
}

impl InstancesModule {
    /// `now`: the module's clock; `None` uses the host's.
    pub fn new(store: Arc<dyn DocumentStore>, now: Option<Clock>) -> Self {
        Self {
            store,
            now,
            state: Mutex::new(State {
                document: None,
                pinned: HashSet::new(),
            }),
        }
    }

    fn load(&self, state: &mut State) -> Result<Value, String> {
        if let Some(document) = &state.document {
            return Ok(document.clone());
        }
        let document = match read_versioned("ultron.module/instances", self.store.read()?, "version")? {
            None => json!({"version": 1, "instances": []}),
            Some(saved) => {
                if saved["version"] != 1 || !saved["instances"].is_array() {
                    return Err("Invalid instance document".into());
                }
                saved
            }
        };
        state.document = Some(document.clone());
        Ok(document)
    }

    fn commit(&self, state: &mut State, next: Value) -> Result<(), String> {
        self.store.write(&next)?;
        state.document = Some(next);
        Ok(())
    }

    fn pin(state: &mut State, instance: &Value, host: &dyn TaskHost) -> bool {
        let id = instance["id"].as_str().unwrap_or("").to_string();
        if state.pinned.contains(&id) {
            return true;
        }
        let ok = host.pin_lane(instance["lane"].as_str().unwrap_or(""), &holder(instance));
        if ok {
            state.pinned.insert(id);
        }
        ok
    }

    fn unpin(state: &mut State, instance: &Value, host: &dyn TaskHost) {
        if state.pinned.remove(instance["id"].as_str().unwrap_or("")) {
            host.unpin_lane(instance["lane"].as_str().unwrap_or(""), &holder(instance));
        }
    }

    fn with_pinned(mut instance: Value, pinned: bool) -> Value {
        instance["pinned"] = pinned.into();
        instance
    }

    fn validate_retain_task(host: &dyn TaskHost, task_id: &str, caller: Option<&str>) -> Result<String, String> {
        let task = host
            .tasks()?
            .into_iter()
            .find(|task| task["id"] == task_id)
            .ok_or("Unknown Ultron task")?;
        if task["parentId"].as_str() != caller {
            return Err("Only the task's parent can retain it".into());
        }
        if task["state"] != "completed" {
            return Err("Only a completed task can be retained".into());
        }
        let definition = task["definition"].as_str().unwrap_or("").to_string();
        if host.strategy(&definition)? != "rlm" {
            return Err("Only RLM tasks have a lane to retain".into());
        }
        Ok(definition)
    }

    fn retain(&self, state: &mut State, current: Value, request: &Request<'_>, now: i64) -> Result<Value, String> {
        kernel::ops::fields("instances.retain", request.payload)?;
        let task_id = text(request.payload.get("task_id"), "task_id")?;
        let existing = current["instances"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["task_id"] == task_id.as_str()))
            .cloned();
        if let Some(existing) = existing {
            assert_owner(&existing, request.caller_task)?;
            let pinned = existing["state"] == "open" && Self::pin(state, &existing, request.host);
            return Ok(Self::with_pinned(existing, pinned));
        }
        let definition = Self::validate_retain_task(request.host, &task_id, request.caller_task)?;
        let lane = request
            .host
            .task_lane(&task_id)
            .unwrap_or_else(|| format!("ultron.{}.{task_id}", definition.split('@').next().unwrap_or("")));
        let record = json!({
            "id": format!("ultron-instance-{}", jsrt::random::uuid_v4()),
            "task_id": task_id,
            "definition": definition,
            "lane": lane,
            "owner": request.caller_task,
            "state": "open",
            "created_at": now,
            "closed_at": null,
            "invocations": [],
        });
        let mut next = current;
        if let Some(Value::Array(items)) = next.get_mut("instances") {
            items.push(record.clone());
        }
        self.commit(state, next)?;
        let pinned = Self::pin(state, &record, request.host);
        Ok(Self::with_pinned(record, pinned))
    }

    fn ensure_idle(host: &dyn TaskHost, instance: &Value) -> Result<(), String> {
        let last = instance["invocations"]
            .as_array()
            .and_then(|items| items.last())
            .map_or_else(|| instance["task_id"].clone(), |item| item["task_id"].clone());
        let last_state = host
            .tasks()?
            .into_iter()
            .find(|task| task["id"] == last)
            .map(|task| task["state"].as_str().unwrap_or("").to_string());
        if last_state.is_some_and(|state| !TERMINAL.contains(&state.as_str())) {
            return Err("Instance is busy with a previous invocation".into());
        }
        Ok(())
    }

    fn build_invoke_spawn<'a>(
        instance: &Value,
        payload: &'a serde_json::Map<String, Value>,
    ) -> Result<(Value, &'a Value), String> {
        let input = payload.get("input").ok_or("input must be JSON")?;
        let mut spawn = json!({
            "definition": instance["definition"],
            "input": input,
            "lane": instance["lane"],
        });
        if let Some(key) = payload.get("key").filter(|key| !key.is_null()) {
            let key = text(Some(key), "key")?;
            spawn["key"] = format!("instance:{}:{key}", instance["id"].as_str().unwrap_or("")).into();
        }
        Ok((spawn, input))
    }

    fn record_invocation(
        &self,
        state: &mut State,
        mut next: Value,
        index: usize,
        task_id: &Value,
        input: &Value,
        now: i64,
    ) -> Result<(), String> {
        let invoked = next["instances"][index]["invocations"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["task_id"] == *task_id));
        if !invoked {
            if let Some(Value::Array(items)) = next["instances"][index].get_mut("invocations") {
                items.push(json!({
                    "task_id": task_id,
                    "at": now,
                    "input_sha256": jsrt::hash::sha256_hex(jsrt::json::stringify(input).as_bytes()),
                }));
            }
            self.commit(state, next)?;
        }
        Ok(())
    }

    fn invoke(&self, state: &mut State, current: Value, request: &Request<'_>, now: i64) -> Result<Value, String> {
        kernel::ops::fields("instances.invoke", request.payload)?;
        let index = find_index(&current, request.payload.get("id"))?;
        let instance = current["instances"][index].clone();
        assert_owner(&instance, request.caller_task)?;
        if instance["state"] != "open" {
            return Err("Instance is closed".into());
        }
        Self::ensure_idle(request.host, &instance)?;
        let (spawn, input) = Self::build_invoke_spawn(&instance, request.payload)?;
        let task = request.host.spawn(&spawn, request.caller_task)?;
        self.record_invocation(state, current, index, &task["id"], input, now)?;
        Ok(json!({"instance_id": instance["id"], "task_id": task["id"]}))
    }

    fn close(&self, state: &mut State, current: Value, request: &Request<'_>, now: i64) -> Result<Value, String> {
        kernel::ops::fields("instances.close", request.payload)?;
        let index = find_index(&current, request.payload.get("id"))?;
        let instance = current["instances"][index].clone();
        assert_owner(&instance, request.caller_task)?;
        if instance["state"] == "closed" {
            return Ok(instance);
        }
        let mut next = current;
        next["instances"][index]["state"] = "closed".into();
        next["instances"][index]["closed_at"] = now.into();
        let closed = next["instances"][index].clone();
        self.commit(state, next)?;
        Self::unpin(state, &instance, request.host);
        Ok(closed)
    }

    fn get(current: &Value, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("instances.get", request.payload)?;
        let index = find_index(current, request.payload.get("id"))?;
        let instance = current["instances"][index].clone();
        assert_owner(&instance, request.caller_task)?;
        Ok(instance)
    }

    fn list(current: &Value, request: &Request<'_>) -> Result<Value, String> {
        kernel::ops::fields("instances.list", request.payload)?;
        Ok(Value::Array(
            current["instances"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|instance| request.caller_task.is_none_or(|caller| instance["owner"] == caller))
                .cloned()
                .collect(),
        ))
    }
}

impl ApiModule for InstancesModule {
    fn prefixes(&self) -> &[&str] {
        &["instances."]
    }

    fn start(&self, host: &dyn TaskHost) -> Result<(), String> {
        let mut state = jsrt::sync::lock(&self.state);
        let document = self.load(&mut state)?;
        for instance in document["instances"].as_array().into_iter().flatten() {
            if instance["state"] == "open" {
                Self::pin(&mut state, instance, host);
            }
        }
        Ok(())
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        let mut state = jsrt::sync::lock(&self.state);
        let current = self.load(&mut state)?;
        let now = self.now.as_ref().map_or_else(|| request.host.now(), |now| now());
        match request.kind {
            "instances.retain" => self.retain(&mut state, current, request, now),
            "instances.invoke" => self.invoke(&mut state, current, request, now),
            "instances.close" => self.close(&mut state, current, request, now),
            "instances.get" => Self::get(&current, request),
            "instances.list" => Self::list(&current, request),
            other => Err(format!("Unknown instance request: {other}")),
        }
    }
}
