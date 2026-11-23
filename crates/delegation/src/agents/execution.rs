use super::*;
use crate::verdict::{MAX_VERDICT_REJECTIONS, validate_verdict};
use jsonschema::json_equal;

pub(super) fn object_input(value: Option<&Value>) -> Result<&Map<String, Value>, String> {
    value
        .and_then(Value::as_object)
        .ok_or_else(|| "agent input must be a JSON object".to_string())
}

/// `jsonFrom(text)`: a reply as JSON, a fenced block unwrapped.
pub(super) fn json_from(text: &str) -> Result<Value, String> {
    let trimmed = string::trim(text);
    let body = trimmed
        .strip_prefix("```")
        .and_then(|rest| {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            let rest = rest.trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
            let rest = rest.strip_prefix('\n')?;
            rest.strip_suffix("\n```")
        })
        .unwrap_or(trimmed);
    serde_json::from_str(body).map_err(|error| {
        let shown = if utf16::len(body) > 32 {
            format!("{}...", utf16::head(body, 32))
        } else {
            body.to_string()
        };
        format!("Unexpected token in JSON: \"{shown}\" is not valid JSON ({error})")
    })
}

pub(super) fn text_of(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// `conditionMet`: the compared field (or whole value) equals `equals`.
pub(super) fn condition_met(value: Option<&Value>, field: Option<&str>, equals: &Value) -> bool {
    let actual = match field {
        None => value,
        Some(field) => value.and_then(Value::as_object).and_then(|map| map.get(field)),
    };
    actual.is_some_and(|actual| json_equal(actual, equals))
}

/// `boundValue`: a subagent's checked verdict when it gave one, else the node's value.
pub(super) fn bound_value(outcome: Option<&Value>) -> Option<Value> {
    let outcome = outcome?;
    let verdict = &outcome["verdict"];
    if !verdict.is_object() {
        return outcome.get("value").cloned();
    }
    Some(json!({
        "status": verdict["status"], "summary": verdict["summary"], "outputs": verdict["outputs"],
        "evidence": verdict["evidence"], "changed_files": verdict["changed_files"],
        "check": outcome["check"]["outcome"].as_str().unwrap_or("unchecked"),
        "reply": outcome["value"].as_str().map_or(Value::Null, |text| json!(text)),
    }))
}

/// `verdictGate`: a subagent node succeeds only when its verdict passed and was not contradicted.
pub(super) fn verdict_gate(result: Value) -> Value {
    let verdict = &result["verdict"];
    if result["status"] != "succeeded" || !verdict.is_object() {
        return result;
    }
    let mut gated = result.clone();
    if verdict["status"] != "passed" {
        gated["status"] = json!("failed");
        gated["error"] = json!(format!(
            "Subagent verdict {}: {}",
            verdict["status"].as_str().unwrap_or_default(),
            verdict["summary"].as_str().unwrap_or_default()
        ));
        return gated;
    }
    if result["check"]["outcome"] == "contradicted" {
        let unobserved: Vec<String> = result["check"]["unobserved"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text_of)
            .collect();
        gated["status"] = json!("failed");
        gated["error"] = json!(format!(
            "Subagent verdict contradicted: declared as changed but unchanged: {}",
            unobserved.join(", ")
        ));
    }
    gated
}

impl Agents {
    /// `execute`: the strategy's work; every failure is a typed result, never a success.
    pub(super) fn execute(&self, id: &str, request: &TaskRequest) -> Value {
        let signal = match self.state().task(id) {
            Some(task) => task.signal.clone(),
            None => return failed("Unknown Ultron task"),
        };
        let outcome = self.work(id, request, &signal);
        match outcome {
            Ok(result) => result,
            Err(error) => {
                if signal.is_aborted() {
                    cancelled(error)
                } else {
                    failed(error)
                }
            }
        }
    }

    fn work(&self, id: &str, request: &TaskRequest, signal: &Signal) -> Result<Value, String> {
        let aborted = || signal.reason().map_or(Ok(()), Err);
        aborted()?;
        let definition = self.definition(&request.definition)?;
        let public = definition.public();
        let output_error = |value: &Value| -> Option<String> {
            let state = self.state();
            (!state.registry.is_valid_output(&definition, value))
                .then(|| state.registry.validation_error(&definition, value, "output"))
        };
        match definition.strategy() {
            Strategy::Deterministic => {
                self.state().registry.can_execute(&definition)?;
                let value = match &self.inner.deterministic {
                    None if definition.key() == "identity@1" => request.input.clone(),
                    None => {
                        return Err(format!(
                            "Deterministic definition {} requires an injected deterministic adapter",
                            definition.key()
                        ));
                    }
                    Some(adapter) => adapter(AdapterRequest {
                        definition: &public,
                        input: &request.input,
                        signal,
                        repair: None,
                    })?,
                };
                aborted()?;
                match output_error(&value) {
                    Some(error) => Err(error),
                    None => Ok(succeeded(value)),
                }
            }
            Strategy::Predict => {
                self.state().registry.can_execute(&definition)?;
                let adapter = self.inner.predict.clone().ok_or_else(|| {
                    format!(
                        "Predict definition {} requires an injected predict adapter",
                        definition.key()
                    )
                })?;
                let mut repair: Option<Value> = None;
                for attempt in 0..=definition.max_repairs() {
                    aborted()?;
                    let key = if attempt == 0 {
                        format!("{id}:model")
                    } else {
                        format!("{id}:model:repair-{attempt}")
                    };
                    let reservation = self.reserve_model(id, key, request)?;
                    let answer = adapter(AdapterRequest {
                        definition: &public,
                        input: &request.input,
                        signal,
                        repair: repair.clone(),
                    });
                    let status = match &answer {
                        Ok(_) => "succeeded",
                        Err(_) if signal.is_aborted() => "cancelled",
                        Err(_) => "failed",
                    };
                    if let Some(usage) = &self.inner.usage {
                        if let Some(reservation) = &reservation {
                            usage.settle(reservation, Some(status), None)?;
                        }
                        usage.record_turn(self.root_of_task(id).as_deref(), None, None)?;
                    }
                    let value = answer?;
                    match output_error(&value) {
                        None => return Ok(succeeded(value)),
                        Some(error) => {
                            repair = Some(json!({ "attempt": attempt + 1, "previous": value, "error": error }));
                        }
                    }
                }
                let error = repair
                    .as_ref()
                    .and_then(|r| r["error"].as_str())
                    .unwrap_or("Predict adapter returned an invalid output")
                    .to_string();
                let repairs = definition.max_repairs();
                Err(match repairs {
                    0 => error,
                    1 => format!("{error} after 1 repair attempt"),
                    n => format!("{error} after {n} repair attempts"),
                })
            }
            Strategy::Rlm => self.run_lane(id, request, &definition, signal),
        }
    }

    /// The `rlm` strategy: the definition's prompt (or a subagent's brief) on its own lane.
    fn run_lane(
        &self,
        id: &str,
        request: &TaskRequest,
        definition: &Definition,
        signal: &Signal,
    ) -> Result<Value, String> {
        if definition.id() == "rlm-frame" {
            return self.run_frame(id, request, signal);
        }
        let runner = self
            .inner
            .lanes
            .clone()
            .ok_or("Ultron rlm definitions need a lane runner, and this host has none")?;
        let lane = request
            .lane
            .clone()
            .unwrap_or_else(|| format!("ultron.{}.{id}", definition.id()));
        if request.lane.is_some()
            && let Some(hook) = &self.inner.before_lane_reuse
        {
            hook(&lane)?;
        }
        let model = request.model.clone().or_else(|| definition.model().map(String::from));
        {
            let mut state = self.state();
            state.lane_tasks.insert(lane.clone(), id.into());
            if let Some(task) = state.task_mut(id) {
                task.lane = Some(lane.clone());
                task.model = model.as_deref().map(|m| json::preview(&json!(m), 80));
            }
        }
        if signal.is_aborted() {
            self.abort_lane(&lane);
        }
        signal.reason().map_or(Ok(()), Err)?;
        let base = if definition.id() == "rlm-child" {
            request.input["prompt"].as_str().unwrap_or_default().to_string()
        } else {
            format!(
                "{}\n\nInput data:\n{}\n\nOutput contract:\n{}",
                definition.instructions(),
                json::stringify(&request.input),
                definition.output_description()
            )
        };
        let refinements = self
            .inner
            .refinements
            .as_ref()
            .and_then(|hook| hook(definition.id()).ok())
            .unwrap_or_default();
        signal.reason().map_or(Ok(()), Err)?;
        let base = match &request.brief {
            Some(brief) => brief.prompt(&base),
            None => base,
        };
        let prompt = if refinements.is_empty() {
            base
        } else {
            let notes: Vec<String> = refinements
                .iter()
                .map(|r| {
                    let version = match &r["version"] {
                        Value::Null => "unversioned".to_string(),
                        other => json::stringify(other),
                    };
                    format!(
                        "Active refinement {} (version {version}):\n{}",
                        r["id"].as_str().unwrap_or_default(),
                        r["text"].as_str().unwrap_or_default()
                    )
                })
                .collect();
            format!("{base}\n\n{}", notes.join("\n\n"))
        };
        let reservation = self.reserve_model(id, format!("{id}:model"), request)?;
        let deadline = reservation.as_ref().and_then(Reservation::deadline_at).or_else(|| {
            self.state()
                .task(id)
                .and_then(|t| t.reservation.as_ref())
                .and_then(Reservation::deadline_at)
        });
        let reply = runner(LaneRun {
            task_id: id.into(),
            lane,
            reuse: request.lane.is_some(),
            prompt,
            model,
            timeout_ms: request.timeout_ms,
            deadline_at: deadline,
            definition: definition.key(),
            signal: signal.clone(),
        });
        let status = match &reply {
            _ if signal.is_aborted() => "cancelled",
            Ok(_) => "succeeded",
            Err(_) => "failed",
        };
        let usage = reply.as_ref().ok().and_then(|r| r.usage.clone());
        {
            let mut state = self.state();
            if let Some(task) = state.task_mut(id) {
                task.cost = usage.as_ref().and_then(|u| u.cost);
                task.tokens = usage.as_ref().and_then(|u| u.total_tokens);
                if let Ok(reply) = &reply {
                    task.turns = Some(task.turns.unwrap_or(0).max(reply.turns));
                    task.tool_calls = Some(task.tool_calls.unwrap_or(0).max(reply.tool_calls));
                    let flat = string::flatten(&reply.text);
                    if !flat.is_empty() {
                        task.last_text = Some(json::preview(&json!(flat), GRAPH_TEXT_CHARS));
                    }
                }
            }
        }
        if let (Some(ledger), Some(reservation)) = (&self.inner.usage, &reservation) {
            ledger.settle(reservation, Some(status), usage.as_ref())?;
        }
        signal.reason().map_or(Ok(()), Err)?;
        let text = reply?.text;
        if definition.id() == "rlm-child" {
            return self.child_result(id, &text);
        }
        if string::trim(&text).is_empty() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if definition.id() == "background-job" {
            json!(text)
        } else {
            json_from(&text)?
        };
        let state = self.state();
        if !state.registry.is_valid_output(definition, &value) {
            return Err(state.registry.validation_error(definition, &value, "output"));
        }
        Ok(succeeded(value))
    }

    /// An `rlm-frame@1` task: its lane acts for it, and the frame runner does the work.
    fn run_frame(&self, id: &str, request: &TaskRequest, signal: &Signal) -> Result<Value, String> {
        let runner = self
            .inner
            .frames
            .clone()
            .ok_or("rlm-frame@1 tasks are started by rlm.infer and rlm.map only")?;
        let lane = lanes::lane_names::frame_lane(id);
        let (usage_root, deadline_at) = {
            let mut state = self.state();
            state.lane_tasks.insert(lane.clone(), id.into());
            let task = state.task_mut(id).ok_or("Unknown Ultron task")?;
            task.lane = Some(lane.clone());
            task.model = request.model.as_deref().map(|m| json::preview(&json!(m), 80));
            let reservation = task.reservation.as_ref();
            (
                reservation.map(|r| r.root_id().to_string()),
                reservation.and_then(Reservation::deadline_at),
            )
        };
        signal.reason().map_or(Ok(()), Err)?;
        let value = runner(FrameRun {
            task_id: id.into(),
            input: request.input.clone(),
            lane,
            model: request.model.clone(),
            usage_root,
            deadline_at,
            timeout_ms: request.timeout_ms,
            signal: signal.clone(),
        })?;
        Ok(succeeded(value))
    }

    /// `childResult` without a workspace to compare: the reply (or the verdict's summary), the verdict, and an
    /// unchecked check when a verdict was given or rejected.
    fn child_result(&self, id: &str, text: &str) -> Result<Value, String> {
        let state = self.state();
        let task = state.task(id).ok_or("Unknown Ultron task")?;
        let verdict = task.verdict.clone();
        if string::trim(text).is_empty() && verdict.is_none() {
            return Err("Agent produced no assistant result at the completed tip".into());
        }
        let value = if string::trim(text).is_empty() {
            verdict.as_ref().map_or(Value::Null, |v| v["summary"].clone())
        } else {
            json!(text)
        };
        let concurrent = self.concurrent_with(&state, task);
        let mut check = json!({
            "outcome": if task.verdict_invalid { "invalid" } else { "unchecked" },
            "unobserved": [], "unreported": [], "unlisted": [], "concurrent": concurrent,
            "reason": "no workspace to compare",
        });
        if task.verdict_invalid {
            check["problems"] = json!(task.verdict_problems);
        }
        let mut result = json!({ "status": "succeeded", "value": value, "verification": "unverified",
                                 "verdict": verdict.clone().unwrap_or(Value::Null) });
        if verdict.is_some() || task.verdict_invalid {
            result["check"] = check;
        }
        if verdict.is_none() {
            result["unverified"] = json!(true);
        }
        Ok(result)
    }

    /// `concurrentWith`: other lane-backed tasks whose runs overlapped this one, outside its own line.
    fn concurrent_with(&self, state: &State, task: &Task) -> Vec<String> {
        let start = task.started_at.unwrap_or(0.0);
        let end = self.now();
        let mut ancestors = Vec::new();
        let mut current = task.parent_id.clone();
        while let Some(id) = current {
            current = state.parent_of(&id);
            ancestors.push(id);
        }
        let descends = |other: &Task| {
            let mut current = other.parent_id.clone();
            while let Some(id) = current {
                if id == task.id {
                    return true;
                }
                current = state.parent_of(&id);
            }
            false
        };
        state
            .tasks
            .iter()
            .filter(|other| other.id != task.id && other.lane.is_some())
            .filter(|other| !other.definition.starts_with("rlm-frame@"))
            .filter(|other| {
                other.started_at.is_some_and(|s| s <= end) && other.ended_at.unwrap_or(f64::INFINITY) >= start
            })
            .filter(|other| !ancestors.contains(&other.id) && !descends(other))
            .map(|other| other.id.clone())
            .collect()
    }

    /// `rlm.finish` from a subagent lane this host started (`agents.spawn` of `rlm-child@1`).
    pub(super) fn finish_request(&self, payload: Map<String, Value>, parent: Option<&str>) -> Result<Value, String> {
        let mut state = self.state();
        let Some(task) = parent.and_then(|id| state.task_mut(id)) else {
            return Err("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply".into());
        };
        if task.definition != "rlm-child@1" {
            return Err("rlm.finish is for subagents started with rlm.spawn; here, answer in your reply".into());
        }
        if task.outcome.is_some() || task.finishing {
            return Err("rlm.finish: this subagent has already ended".into());
        }
        if task.verdict_invalid {
            return Err("rlm.finish: no attempts left; end your turn, your reply is returned unverified".into());
        }
        match validate_verdict(&payload, None) {
            Ok(verdict) => {
                let status = verdict["status"].clone();
                task.verdict = Some(verdict);
                Ok(json!({ "recorded": true, "status": status,
                    "next": "End your turn now with a short reply; the host checks changed_files against the files that changed while you ran." }))
            }
            Err(problems) => {
                task.verdict_rejections += 1;
                task.verdict_problems = problems.clone();
                let left = (MAX_VERDICT_REJECTIONS + 1).saturating_sub(task.verdict_rejections);
                let listed = problems.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n");
                if left > 0 || task.verdict.is_some() {
                    let tail = if task.verdict.is_none() {
                        format!(" ({left} attempt{} left)", if left == 1 { "" } else { "s" })
                    } else {
                        "; your earlier verdict stands until then".into()
                    };
                    return Err(format!(
                        "rlm.finish rejected:\n{listed}\nFix these and call rlm.finish again{tail}."
                    ));
                }
                task.verdict_invalid = true;
                Err(format!(
                    "rlm.finish rejected:\n{listed}\nNo attempts left: end your turn; your reply is returned unverified, with these problems."
                ))
            }
        }
    }
}
