//! Error types, diagnostics, and HTTP / event-stream exception decoding for Amazon Bedrock.

use crate::aws::{EventMessage, is_throttling_error, is_transient_error};
use crate::http::HeaderMap;
use crate::provider::{ProviderError, REQUEST_ABORTED};
use crate::types::AssistantMessageDiagnostic;
use jsrt::json_parse::parse_json;
use serde_json::{Map, Value, json};

const DATA_RETENTION_DOCS_URL: &str = "https://docs.aws.amazon.com/bedrock/latest/userguide/data-retention.html";
const DESERIALIZATION_HINT: &str =
    "\n  Deserialization error: to see the raw response, inspect the hidden field {error}.$response on this object.";
const MAX_DIAGNOSTIC_VALUE_CHARS: usize = 200;

/// A failure as upstream's error objects describe it.
#[derive(Debug, Default)]
pub(super) struct BedrockError {
    pub(super) name: String,
    pub(super) message: String,
    /// A `BedrockRuntimeServiceException` (its name is mapped to a readable prefix).
    pub(super) service: bool,
    /// `$metadata.httpStatusCode` / `$metadata.requestId`.
    pub(super) status: Option<u16>,
    pub(super) request_id: Option<String>,
    /// Classified as transient by the SDK retry strategy without a status (connection failures).
    pub(super) transient: bool,
}

impl ProviderError for BedrockError {
    fn plain(message: String) -> Self {
        BedrockError::plain(message)
    }

    fn aborted() -> Self {
        abort_error()
    }
}

impl BedrockError {
    pub(super) fn plain(message: impl Into<String>) -> Self {
        BedrockError {
            name: "Error".into(),
            message: message.into(),
            ..Default::default()
        }
    }

    pub(super) fn retryable(&self) -> bool {
        self.transient || is_throttling_error(&self.name, self.status) || is_transient_error(&self.name, self.status)
    }

    pub(super) fn throttling(&self) -> bool {
        is_throttling_error(&self.name, self.status)
    }
}

pub(super) fn abort_error() -> BedrockError {
    BedrockError {
        name: "AbortError".into(),
        message: REQUEST_ABORTED.into(),
        ..Default::default()
    }
}

fn error_prefix(name: &str) -> &str {
    match name {
        "InternalServerException" => "Internal server error",
        "ModelStreamErrorException" => "Model stream error",
        "ValidationException" => "Validation error",
        "ThrottlingException" => "Throttling error",
        "ServiceUnavailableException" => "Service unavailable",
        other => other,
    }
}

/// `formatBedrockError`.
pub(super) fn format_error(error: &BedrockError) -> String {
    let hint = if error.message.to_lowercase().contains("data retention mode") {
        format!(" See {DATA_RETENTION_DOCS_URL} for supported data retention modes.")
    } else {
        String::new()
    };
    if error.service {
        format!("{}: {}{hint}", error_prefix(&error.name), error.message)
    } else {
        format!("{}{hint}", error.message)
    }
}

pub(super) fn diagnostic_value(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if !trimmed.is_empty() && trimmed.encode_utf16().count() <= MAX_DIAGNOSTIC_VALUE_CHARS {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// `appendBedrockFailureDiagnostic`.
pub(super) fn failure_diagnostic(
    error: &BedrockError,
    fallback_request_id: Option<&str>,
) -> Option<AssistantMessageDiagnostic> {
    let mut details = Map::new();
    if let Some(status) = error.status {
        details.insert("status".into(), json!(status));
    }
    if error.name.ends_with("Exception")
        && let Some(code) = diagnostic_value(Some(&error.name))
    {
        details.insert("errorCode".into(), json!(code));
    }
    let resolved_id = diagnostic_value(error.request_id.as_deref());
    let fallback_id = if resolved_id.is_some() {
        resolved_id
    } else {
        fallback_request_id.map(str::to_string)
    };
    if let Some(id) = fallback_id {
        details.insert("requestId".into(), json!(id));
    }
    if details.is_empty() {
        return None;
    }
    Some(AssistantMessageDiagnostic {
        kind: "bedrock_response_failure".into(),
        timestamp: jsrt::date::now_ms(),
        error: None,
        details: Some(details),
    })
}

/// `sanitizeErrorCode` + the `#` split of `getErrorSchemaOrThrowBaseException`.
fn sanitize_error_code(raw: &str) -> String {
    let mut code = raw;
    if let Some((first, _)) = code.split_once(',') {
        code = first;
    }
    if let Some((first, _)) = code.split_once(':') {
        code = first;
    }
    if let Some((_, after)) = code.split_once('#') {
        code = after.split('#').next().unwrap_or_default();
    }
    code.to_string()
}

pub(super) fn request_id(headers: &HeaderMap) -> Option<String> {
    ["x-amzn-requestid", "x-amzn-request-id", "x-amz-request-id"]
        .iter()
        .find_map(|name| headers.get(*name).cloned())
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => jsrt::json::stringify(other),
    }
}

fn json_field_ci<'a>(object: Option<&'a Map<String, Value>>, key: &str) -> Option<&'a Value> {
    object.and_then(|o| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v))
}

fn resolve_error_code(headers: &HeaderMap, object: Option<&Map<String, Value>>) -> String {
    if let Some(header) = headers.get("x-amzn-errortype") {
        return sanitize_error_code(header);
    }
    if let Some(code) = json_field_ci(object, "code").filter(|v| !v.is_null()) {
        return sanitize_error_code(&value_text(code));
    }
    if let Some(kind) = object.and_then(|o| o.get("__type")).filter(|v| !v.is_null()) {
        return sanitize_error_code(&value_text(kind));
    }
    "Unknown".into()
}

fn resolve_error_message(object: Option<&Map<String, Value>>) -> String {
    let Some(obj) = object else {
        return "UnknownError".into();
    };
    let candidate = obj
        .get("message")
        .filter(|v| !v.is_null())
        .or(obj.get("Message").filter(|v| !v.is_null()));
    match candidate {
        Some(value) => value_text(value),
        None => "UnknownError".into(),
    }
}

/// A non-2xx response as the restJson1 protocol turns it into an exception.
pub(super) fn http_error(status: u16, headers: &HeaderMap, body: &str) -> BedrockError {
    let request_id = request_id(headers);
    let data = if body.is_empty() {
        Value::Object(Map::new())
    } else {
        match parse_json(body) {
            Ok(value) => value,
            Err(_) => {
                return BedrockError {
                    name: "SyntaxError".into(),
                    message: format!(
                        "{}{DESERIALIZATION_HINT}",
                        jsrt::json::parse(body).err().unwrap_or_default()
                    ),
                    status: Some(status),
                    request_id,
                    ..Default::default()
                };
            }
        }
    };
    let object = data.as_object();
    BedrockError {
        name: resolve_error_code(headers, object),
        message: resolve_error_message(object),
        service: true,
        status: Some(status),
        request_id,
        ..Default::default()
    }
}

/// Modeled stream exceptions (members of `ConverseStreamOutput`).
fn modeled_stream_exception(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "internalServerException" => "InternalServerException",
        "modelStreamErrorException" => "ModelStreamErrorException",
        "validationException" => "ValidationException",
        "throttlingException" => "ThrottlingException",
        "serviceUnavailableException" => "ServiceUnavailableException",
        _ => return None,
    })
}

fn message_header(message: &EventMessage, name: &str) -> String {
    message.headers.get(name).cloned().unwrap_or_default()
}

/// An `exception` or `error` message of the event stream as the SDK throws it. `first` marks the message the SDK
/// reads while still inside the request (its errors get the deserialization hint and HTTP metadata).
pub(super) fn stream_error(
    message: &EventMessage,
    first: bool,
    response_status: u16,
    response_id: Option<&str>,
) -> BedrockError {
    let mut error = if message_header(message, ":message-type") == "error" {
        let text = message_header(message, ":error-message");
        BedrockError {
            name: message_header(message, ":error-code"),
            message: if text.is_empty() { "UnknownError".into() } else { text },
            ..Default::default()
        }
    } else {
        let kind = message_header(message, ":exception-type");
        if let Some(name) = modeled_stream_exception(&kind) {
            let body = parse_json(&String::from_utf8_lossy(&message.body)).unwrap_or(Value::Null);
            return BedrockError {
                name: name.into(),
                message: resolve_error_message(body.as_object()),
                service: true,
                ..Default::default()
            };
        }
        BedrockError {
            name: kind,
            message: String::from_utf8_lossy(&message.body).into_owned(),
            ..Default::default()
        }
    };
    if first {
        error.message.push_str(DESERIALIZATION_HINT);
        error.status = Some(response_status);
        error.request_id = response_id.map(str::to_string);
    }
    error
}
