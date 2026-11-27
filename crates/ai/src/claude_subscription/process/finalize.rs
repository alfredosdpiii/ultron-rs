//! Finalization and reconciliation of a completed `claude -p` turn against the admission relay's captured response.

use super::super::admission::Outcome;
use super::super::history::TOOL;
use super::super::limit::{self, Signals};
use super::super::{ClaudeError, LOGGED_OUT_HINT};
use super::{
    Finish, NativeCall, NativeResponse, Prepared, StreamOutcome, extract_calls, native_error_status, python_str,
    text_of,
};
use serde_json::{Value, json};

fn incomplete_upstream_error(
    outcome: &Outcome,
    native_error: Option<&(String, Option<String>)>,
    signals: &Signals,
) -> ClaudeError {
    let complete = outcome.capture.complete;
    let mut first = format!(
        "first upstream attempt: status {}, capture {}",
        outcome.status.map_or("None".into(), |s| s.to_string()),
        if complete { "complete" } else { "incomplete" }
    );
    if let Some(failure) = &outcome.failure {
        first.push_str(&format!(", relay failure {failure}"));
    }
    first.push_str(&format!(", native retries denied: {}", outcome.denied));
    let said = outcome.error_text();
    if !said.is_empty() {
        first.push_str(&format!(
            ", upstream said: {}",
            said.chars().take(500).collect::<String>()
        ));
    }
    let mut message = format!("Incomplete upstream response ({first})");
    if let Some((detail, _)) = native_error {
        message.push_str(&format!(": {detail}"));
    }
    let status = outcome.status.filter(|s| *s >= 400);
    limit::classify(ClaudeError::Api { message, status }, signals)
}

fn classify_native_error(
    detail: &str,
    code: Option<&str>,
    used: bool,
    err_tail: &str,
    signals: &Signals,
) -> ClaudeError {
    if code == Some("authentication_failed") && !used {
        return ClaudeError::LoggedOut(format!("{LOGGED_OUT_HINT} (native: {detail}){err_tail}"));
    }
    let error = ClaudeError::Api {
        message: format!("Native API error: {detail}{err_tail}"),
        status: code.and_then(native_error_status),
    };
    limit::classify(error, signals)
}

fn validate_native_result(
    last_result: &Value,
    calls: &[NativeCall],
    code: Option<i32>,
    handled: bool,
    err_tail: &str,
) -> Result<(), ClaudeError> {
    let subtype = last_result.get("subtype");
    let boundary = !calls.is_empty() && subtype == Some(&json!("error_max_turns")) && code == Some(1);
    if !boundary
        && !handled
        && (code != Some(0) || jsrt::value::truthy(last_result.get("is_error")) || subtype != Some(&json!("success")))
    {
        return Err(ClaudeError::Failed(format!(
            "Native request failed: {}{err_tail}",
            python_str(subtype)
        )));
    }
    Ok(())
}

fn compute_text_tail(blocks: &[Value], emitted: &str) -> Result<String, ClaudeError> {
    let text = text_of(blocks, "text", "text");
    if emitted == text {
        Ok(String::new())
    } else if let Some(rest) = text.strip_prefix(emitted) {
        Ok(rest.to_string())
    } else {
        Err(ClaudeError::Failed(
            "Native final text differs from incremental stream".into(),
        ))
    }
}

fn extract_refusal(assistants: &[Value]) -> Option<String> {
    let details = assistants
        .iter()
        .find_map(|a| a.get("stop_details").filter(|d| d.is_object()))
        .cloned()
        .unwrap_or(json!({}));
    let explanation = details["explanation"].as_str().map(str::trim).filter(|e| !e.is_empty());
    let category = details["category"].as_str().filter(|c| !c.is_empty());
    explanation
        .map(str::to_string)
        .or_else(|| category.map(|c| format!("provider refusal category: {c}")))
}

fn determine_finish(refused: bool, calls: &[NativeCall], assistants: &[Value]) -> Finish {
    if refused {
        return Finish::ContentFilter;
    }
    if !calls.is_empty() {
        return Finish::ToolCalls;
    }
    let length = assistants.iter().any(|a| {
        matches!(
            a["stop_reason"].as_str(),
            Some("max_tokens" | "model_context_window_exceeded")
        )
    });
    if length { Finish::Length } else { Finish::Stop }
}

fn build_admission_report(outcome: &Outcome) -> Value {
    let mut report = json!({
        "upstream_requests": u64::from(outcome.used),
        "blocked_requests": outcome.denied,
        "request_id": outcome.request_id,
    });
    if let Some(unrestored) = &outcome.unrestored {
        report["unrestored"] = json!(unrestored);
    }
    report
}

fn reconcile_refusal_and_calls(
    assistants: &[Value],
    calls: &mut Vec<NativeCall>,
) -> Result<(bool, Option<String>), ClaudeError> {
    let refused = assistants.iter().any(|a| a["stop_reason"] == "refusal");
    if refused {
        calls.clear();
        return Ok((true, extract_refusal(assistants)));
    }
    if let Some(other) = calls.iter().find(|c| c.name != TOOL) {
        return Err(ClaudeError::Failed(format!(
            "The model called `{}`, which is not an Ultron tool: claude-subscription advertises exactly one tool, `{TOOL}`",
            other.name
        )));
    }
    Ok((false, None))
}

struct ReconciledTurn {
    assistants: Vec<Value>,
    last_result: Value,
    handled: bool,
    emitted: String,
}

fn apply_captured_outcome(
    outcome: &Outcome,
    native_error: Option<&(String, Option<String>)>,
    signals: &Signals,
    assistants: &mut Vec<Value>,
    stopped: &mut bool,
) -> Result<(), ClaudeError> {
    if !outcome.used {
        return Ok(());
    }
    if outcome.status != Some(200) || !outcome.capture.complete {
        return Err(incomplete_upstream_error(outcome, native_error, signals));
    }
    *assistants = vec![outcome.capture.message.clone().unwrap_or(Value::Null)];
    *stopped = true;
    Ok(())
}

fn reconcile_turn(stream: StreamOutcome, outcome: &Outcome, err_tail: &str) -> Result<ReconciledTurn, ClaudeError> {
    let StreamOutcome {
        mut assistants,
        mut results,
        mut stopped,
        emitted,
        native_error,
        mut signals,
        ..
    } = stream;
    signals.header_status = outcome.unified_status.clone();
    signals.header_reset = outcome.unified_reset.clone();
    signals.native_code = native_error.as_ref().and_then(|(_, code)| code.clone());
    apply_captured_outcome(outcome, native_error.as_ref(), &signals, &mut assistants, &mut stopped)?;
    let handled = outcome.denied > 0 || (outcome.used && assistants[0]["stop_reason"] == "refusal");
    if let Some((detail, err_code)) = &native_error
        && !handled
    {
        return Err(classify_native_error(
            detail,
            err_code.as_deref(),
            outcome.used,
            err_tail,
            &signals,
        ));
    }
    if results.len() != 1 || assistants.is_empty() || !stopped {
        return Err(ClaudeError::Failed(format!(
            "Incomplete native response: assistant, message_stop and one result required{err_tail}"
        )));
    }
    Ok(ReconciledTurn {
        assistants,
        last_result: results.remove(0),
        handled,
        emitted,
    })
}

fn extract_usage(outcome_used: bool, assistants: &[Value], last_result: &Value) -> Result<Value, ClaudeError> {
    let usage = if outcome_used {
        assistants[0]["usage"].clone()
    } else {
        last_result["usage"].clone()
    };
    if !usage["input_tokens"].is_number() || !usage["output_tokens"].is_number() {
        return Err(ClaudeError::Failed("Native result missing complete token usage".into()));
    }
    Ok(usage)
}

pub(super) fn finalize_response(
    stream: StreamOutcome,
    outcome: &Outcome,
    prepared: &Prepared,
    code: Option<i32>,
    err_tail: &str,
    warnings: Vec<String>,
) -> Result<NativeResponse, ClaudeError> {
    let ReconciledTurn {
        mut assistants,
        last_result,
        handled,
        emitted,
    } = reconcile_turn(stream, outcome, err_tail)?;
    let mut calls = extract_calls(&mut assistants, &prepared.body.names);
    validate_native_result(&last_result, &calls, code, handled, err_tail)?;
    let usage = extract_usage(outcome.used, &assistants, &last_result)?;
    let blocks: Vec<Value> = assistants
        .iter()
        .flat_map(|a| a["content"].as_array().cloned().unwrap_or_default())
        .collect();
    let tail = compute_text_tail(&blocks, &emitted)?;
    let (refused, refusal) = reconcile_refusal_and_calls(&assistants, &mut calls)?;
    let finish = determine_finish(refused, &calls, &assistants);
    let cost = json!({
        "total_cost_usd": last_result.get("total_cost_usd").cloned().unwrap_or(Value::Null),
        "modelUsage": last_result.get("modelUsage").cloned().unwrap_or(Value::Null),
    });
    Ok(NativeResponse {
        assistants,
        blocks,
        calls,
        refusal,
        finish,
        usage,
        cost,
        admission: build_admission_report(outcome),
        tail,
        warnings,
    })
}
