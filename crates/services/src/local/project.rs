use super::{Branch, LocalServices, escalation, fields, identity};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

/// The conversation branch refinements follow (`mainBranch`): the main lane's tip and which anchors are on its
/// path, read when a request is made.
pub type BranchSource = Arc<dyn Fn() -> Branch + Send + Sync>;

/// An active refinement record as prompts show it: `{id, version, text}`, text content as is, other content as its
/// JSON.
pub fn refinement_view(record: &Value) -> Value {
    let text = match &record["content"] {
        Value::String(text) => text.clone(),
        other => jsrt::json::stringify(other),
    };
    json!({"id": record["id"], "version": record["version"], "text": text})
}

/// A definition id's active instruction refinements, `[{id, version, text}]`.
pub type InstructionHook = Arc<dyn Fn(&str) -> Result<Vec<Value>, String> + Send + Sync>;

/// Checks all host-enforced refinement guardrails: valid kind/target (no protected authority), no capability
/// escalation in structured content or YAML frontmatter, and domain content validation.
pub fn check_refinement_safety(kind: &str, target: &str, content: &Value) -> Result<(), String> {
    let (kind, _) = identity(&json!(kind), &json!(target))?;
    if let Some(key) = escalation(content) {
        return Err(format!("Refinement content cannot request capabilities ({key})"));
    }
    if !crate::validation::validate_refinement_content(&kind, content)? {
        return Err("Refinement content validation failed".into());
    }
    Ok(())
}

/// Active project-scoped refinements from `memory`, re-checked by [`check_refinement_safety`] on read (defense in
/// depth against externally modified memory). Returns an empty list on outage or unsupported backends.
pub fn safe_project_refinements(memory: &kernel_apis::MemoryService) -> Vec<Value> {
    if !memory.supports_project_refinements() {
        return Vec::new();
    }
    let signal = kernel::Signal::new();
    memory
        .list_project_refinements(&signal)
        .unwrap_or_default()
        .into_iter()
        .filter(|record| {
            let Some(kind) = record["kind"].as_str() else {
                return false;
            };
            let Some(target) = record["target"].as_str() else {
                return false;
            };
            check_refinement_safety(kind, target, &record["content"]).is_ok()
        })
        .collect()
}

/// The active instruction refinements of a definition id as a typed-agent host's refinement hook
/// (`refinements` of `NativeHostOptions`): `[{id, version, text}]` of `instruction:<definition id>` on the branch
/// `branch` names (`mainBranch`; every event without one).
pub fn instruction_hook(services: Arc<LocalServices>, branch: Option<BranchSource>) -> InstructionHook {
    instruction_hook_with_memory(services, branch, None)
}

/// [`instruction_hook`] with optional project-scoped fallback from memorantado (`memory`): an active session
/// refinement on the current branch overrides a project refinement for the same definition id.
pub fn instruction_hook_with_memory(
    services: Arc<LocalServices>,
    branch: Option<BranchSource>,
    memory: Option<Arc<kernel_apis::MemoryService>>,
) -> InstructionHook {
    Arc::new(move |definition: &str| {
        let target = format!("instruction:{definition}");
        let branch = branch.as_ref().map(|source| source());
        if let Some(record) = services.current("instruction", &target, branch.as_ref())? {
            return Ok(vec![refinement_view(&record)]);
        }
        if let Some(memory) = &memory
            && let Some(project) = safe_project_refinements(memory)
                .into_iter()
                .find(|r| r["kind"] == "instruction" && r["target"] == target)
        {
            return Ok(vec![refinement_view(&project)]);
        }
        Ok(Vec::new())
    })
}

/// `refinements.*` and `experiments.*` as a host module. With a `branch`, refinements follow the conversation branch
/// (session-worker.ts `branchedServices`): a change is anchored at the main lane's tip and read back only while that
/// anchor is on the branch in use; without one every refinement event applies.
pub struct LocalServicesModule {
    pub services: Arc<LocalServices>,
    pub branch: Option<BranchSource>,
}

impl kernel_apis::ApiModule for LocalServicesModule {
    fn prefixes(&self) -> &[&str] {
        &["refinements.", "experiments."]
    }

    fn handle(&self, request: &kernel_apis::Request<'_>) -> Result<Value, String> {
        let branch = match &self.branch {
            Some(source) if request.kind.starts_with("refinements.") => Some(source()),
            _ => None,
        };
        self.services
            .handle(request.kind, &Value::Object(request.payload.clone()), branch.as_ref())
    }
}

/// [`LocalServicesModule`] extended with memorantado project-scoped persistence (`scope: "project"`,
/// `refinements.promote`, project rollback via `project:<claim-id>`, and merged `list` / `current`).
struct ProjectServicesModule {
    services: Arc<LocalServices>,
    branch: Option<BranchSource>,
    memory: Option<Arc<kernel_apis::MemoryService>>,
}

/// Builds the host module for `refinements.*` and `experiments.*`, backed by `memory` for project-scoped refinements
/// when `memory` is a curating backend (memorantado).
pub fn local_module(
    services: Arc<LocalServices>,
    branch: Option<BranchSource>,
    memory: Option<Arc<kernel_apis::MemoryService>>,
) -> Arc<dyn kernel_apis::ApiModule> {
    Arc::new(ProjectServicesModule {
        services,
        branch,
        memory,
    })
}

fn retire_project_by_target(memory: &kernel_apis::MemoryService, target: &str) {
    let signal = kernel::Signal::new();
    for project in safe_project_refinements(memory) {
        if project["target"] == target
            && let Some(claim_id) = project["claimId"].as_str()
        {
            let _ = memory.retire_project_refinement(claim_id, "refinements.rollback", &signal);
        }
    }
}

fn merge_project_refinements(records: &mut Vec<Value>, memory: &kernel_apis::MemoryService) {
    let session_targets: HashSet<String> = records
        .iter()
        .filter(|r| matches!(r["state"].as_str(), Some("active" | "rolled_back")))
        .filter_map(|r| r["target"].as_str().map(str::to_string))
        .collect();
    for project in safe_project_refinements(memory) {
        if project["target"].as_str().is_some_and(|t| !session_targets.contains(t)) {
            records.push(project);
        }
    }
}

impl ProjectServicesModule {
    fn require_project_memory(&self) -> Result<&kernel_apis::MemoryService, String> {
        let Some(memory) = self.memory.as_deref() else {
            return Err("Project refinements require a curating memory backend (memorantado)".into());
        };
        if !memory.supports_project_refinements() {
            return Err("Project refinements require a curating memory backend (memorantado)".into());
        }
        Ok(memory)
    }

    fn promote(&self, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let map = fields(payload, &["id"], &[])?;
        let memory = self.require_project_memory()?;
        let record = self
            .services
            .handle("refinements.get", &json!({"id": map["id"]}), branch)?;
        if record["state"] != "active" {
            return Err("Only an active refinement can be promoted".into());
        }
        let kind = record["kind"].as_str().unwrap_or_default();
        let target = record["target"].as_str().unwrap_or_default();
        check_refinement_safety(kind, target, &record["content"])?;
        memory
            .save_project_refinement(&record, &kernel::Signal::new())
            .map_err(|e| format!("Failed to save project refinement: {e:?}"))
    }

    fn activate(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let record = self.services.handle(kind, payload, branch)?;
        if record["scope"] == "project"
            && let Some(memory) = &self.memory
            && memory.supports_project_refinements()
        {
            let _ = memory.save_project_refinement(&record, &kernel::Signal::new());
        }
        Ok(record)
    }

    fn rollback_project_id(&self, id: &str) -> Result<Value, String> {
        let Some(memory) = &self.memory else {
            return Err(format!("Unknown refinement: {id}"));
        };
        memory
            .retire_project_refinement(id, "refinements.rollback", &kernel::Signal::new())
            .map_err(|e| format!("Failed to retire project refinement: {e:?}"))?;
        Ok(json!({"id": id, "state": "rolled_back", "scope": "project"}))
    }

    fn rollback(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let map = fields(payload, &["id"], &[])?;
        if let Some(id) = map["id"].as_str()
            && id.starts_with(kernel_apis::memory::PROJECT_REFINEMENT_PREFIX)
        {
            return self.rollback_project_id(id);
        }
        let record = self.services.handle(kind, payload, branch)?;
        if record["scope"] == "project"
            && let Some(memory) = &self.memory
            && let Some(target) = record["target"].as_str()
        {
            retire_project_by_target(memory, target);
        }
        Ok(record)
    }

    fn list(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let mut records = self
            .services
            .handle(kind, payload, branch)?
            .as_array()
            .cloned()
            .unwrap_or_default();
        if let Some(memory) = &self.memory {
            merge_project_refinements(&mut records, memory);
        }
        Ok(Value::Array(records))
    }

    fn current(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let current = self.services.handle(kind, payload, branch)?;
        if !current.is_null() {
            return Ok(current);
        }
        if let Some(memory) = &self.memory
            && let Some(req_kind) = payload["kind"].as_str()
            && let Some(target) = payload["target"].as_str()
            && let Some(project) = safe_project_refinements(memory)
                .into_iter()
                .find(|r| r["kind"] == req_kind && r["target"] == target)
        {
            return Ok(project);
        }
        Ok(Value::Null)
    }
}

impl kernel_apis::ApiModule for ProjectServicesModule {
    fn prefixes(&self) -> &[&str] {
        &["refinements.", "experiments."]
    }

    fn handle(&self, request: &kernel_apis::Request<'_>) -> Result<Value, String> {
        let branch = match &self.branch {
            Some(source) if request.kind.starts_with("refinements.") => Some(source()),
            _ => None,
        };
        let payload = Value::Object(request.payload.clone());
        match request.kind {
            "refinements.promote" => self.promote(&payload, branch.as_ref()),
            "refinements.activate" => self.activate(request.kind, &payload, branch.as_ref()),
            "refinements.rollback" => self.rollback(request.kind, &payload, branch.as_ref()),
            "refinements.list" => self.list(request.kind, &payload, branch.as_ref()),
            "refinements.current" => self.current(request.kind, &payload, branch.as_ref()),
            _ => self.services.handle(request.kind, &payload, branch.as_ref()),
        }
    }
}
