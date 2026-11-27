//! AWS SSO credentials (`fromSSO`) and SSO-session OIDC token refresh (`fromSso`).

use super::ini::{home_dir, is_sso_profile, load_sso_sessions, parse_known_files};
use super::{
    ChainInput, CredentialError, Reply, Result, configured_endpoint, js_string, json_body, parse_js_date,
    regional_endpoint, request, str_field,
};
use crate::aws::{AwsCredentials, escape_uri};
use crate::env::ProviderEnv;
use crate::http::HeaderMap;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const SSO_REFRESH: &str = "To refresh this SSO session run aws sso login with the corresponding profile.";
const TOKEN_REFRESH: &str = "To refresh this SSO session run 'aws sso login' with the corresponding profile.";
const EXPIRE_WINDOW_MS: i64 = 5 * 60 * 1000;

/// `getSSOTokenFilepath`.
pub fn sso_token_path(id: &str, env: Option<&ProviderEnv>) -> PathBuf {
    home_dir(env)
        .join(".aws/sso/cache")
        .join(format!("{}.json", sha1_hex(id.as_bytes())))
}

fn read_sso_token(id: &str, env: Option<&ProviderEnv>) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(sso_token_path(id, env)).ok()?;
    match serde_json::from_str(&text).ok()? {
        Value::Object(map) => Some(map),
        _ => Some(Map::new()),
    }
}

/// `lastRefreshAttemptTimes`: one refresh attempt per sso-session every 30 seconds, process-wide.
fn last_refresh_attempts() -> &'static Mutex<HashMap<String, i64>> {
    static ATTEMPTS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    ATTEMPTS.get_or_init(Default::default)
}

struct SsoProfile {
    start_url: String,
    account_id: String,
    region: String,
    role_name: String,
    has_session: bool,
}

fn load_sso_profile(input: &ChainInput<'_>, name: &str) -> Result<SsoProfile> {
    let profiles = parse_known_files(input.env);
    let Some(section) = profiles.get(name) else {
        return Err(CredentialError::next(format!("Profile {name} was not found.")));
    };
    if !is_sso_profile(Some(section)) {
        return Err(CredentialError::next(format!(
            "Profile {name} is not configured with SSO credentials."
        )));
    }
    let mut profile: IndexMap<String, Option<String>> =
        section.iter().map(|(k, v)| (k.clone(), Some(v.clone()))).collect();
    if let Some(session_name) = section.get("sso_session") {
        let sessions = load_sso_sessions(input.env);
        let Some(session) = sessions.get(session_name) else {
            return Err(CredentialError::plain(
                "TypeError",
                "Cannot read properties of undefined (reading 'sso_region')",
            ));
        };
        profile.insert("sso_region".into(), session.get("sso_region").cloned());
        profile.insert("sso_start_url".into(), session.get("sso_start_url").cloned());
    }
    let field = |key: &str| profile.get(key).cloned().flatten().filter(|v| !v.is_empty());
    let (Some(start_url), Some(account_id), Some(region), Some(role_name)) = (
        field("sso_start_url"),
        field("sso_account_id"),
        field("sso_region"),
        field("sso_role_name"),
    ) else {
        return Err(CredentialError::stop(format!(
            "Profile is configured with invalid SSO credentials. Required parameters \"sso_account_id\", \"sso_region\", \"sso_role_name\", \"sso_start_url\". Got {}\nReference: https://docs.aws.amazon.com/cli/latest/userguide/cli-configure-sso.html",
            profile.keys().cloned().collect::<Vec<_>>().join(", ")
        )));
    };
    Ok(SsoProfile {
        start_url,
        account_id,
        region,
        role_name,
        has_session: field("sso_session").is_some(),
    })
}

fn legacy_sso_access_token(input: &ChainInput<'_>, start_url: &str) -> Result<String> {
    let token = read_sso_token(start_url, input.env).ok_or_else(|| {
        CredentialError::stop(format!(
            "The SSO session associated with this profile is invalid. {SSO_REFRESH}"
        ))
    })?;
    let expires = token
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(jsrt::date::parse_date);
    if expires.is_some_and(|ms| ms - input.now_ms <= 0) {
        return Err(CredentialError::stop(format!(
            "The SSO session associated with this profile has expired. {SSO_REFRESH}"
        )));
    }
    Ok(token
        .get("accessToken")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

fn parse_sso_error(reply: &Reply, body: &Value) -> CredentialError {
    let code = reply
        .headers
        .get("x-amzn-errortype")
        .map(|t| t.split(':').next().unwrap_or_default().to_string())
        .or_else(|| str_field(body, "__type").map(|t| t.rsplit('#').next().unwrap_or_default().to_string()))
        .or_else(|| str_field(body, "code"))
        .unwrap_or_else(|| "UnknownError".into());
    let message = str_field(body, "message")
        .or_else(|| str_field(body, "Message"))
        .unwrap_or_else(|| "UnknownError".into());
    CredentialError::stop(format!("{code}: {message}"))
}

fn parse_sso_role_credentials(input: &ChainInput<'_>, body: &Value) -> Result<AwsCredentials> {
    let role = body.get("roleCredentials").cloned().unwrap_or(Value::Null);
    let present = |key: &str| role.get(key).filter(|v| !v.is_null() && *v != "" && *v != 0).cloned();
    match (
        present("accessKeyId"),
        present("secretAccessKey"),
        present("sessionToken"),
        present("expiration"),
    ) {
        (
            Some(Value::String(access_key_id)),
            Some(Value::String(secret_access_key)),
            Some(Value::String(token)),
            Some(expiration),
        ) => {
            input.expiration.set(parse_js_date(&expiration));
            Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token: Some(token),
            })
        }
        _ => Err(CredentialError::stop("SSO returns an invalid temporary credential.")),
    }
}

/// `fromSSO` for a profile.
pub(super) async fn from_sso(input: &ChainInput<'_>, name: &str) -> Result<AwsCredentials> {
    let profile = load_sso_profile(input, name)?;
    let access_token = if profile.has_session {
        sso_session_token(input, name).await.map_err(CredentialError::stop)?
    } else {
        legacy_sso_access_token(input, &profile.start_url)?
    };
    let endpoint =
        configured_endpoint(input, "SSO").unwrap_or_else(|| regional_endpoint("portal.sso", &profile.region));
    let url = format!(
        "{endpoint}/federation/credentials?account_id={}&role_name={}",
        escape_uri(&profile.account_id),
        escape_uri(&profile.role_name)
    );
    let mut headers = HeaderMap::new();
    headers.insert("x-amz-sso_bearer_token".into(), access_token);
    let reply = request(input, "GET", url, headers, String::new(), None)
        .await
        .map_err(|e| CredentialError::stop(format!("Error: {e}")))?;
    let body = json_body(&reply.body);
    if !(200..300).contains(&reply.status) {
        return Err(parse_sso_error(&reply, &body));
    }
    parse_sso_role_credentials(input, &body)
}

fn load_sso_session_region(
    input: &ChainInput<'_>,
    profile_name: &str,
) -> std::result::Result<(String, String), String> {
    let profiles = parse_known_files(input.env);
    let Some(profile) = profiles.get(profile_name) else {
        return Err(format!(
            "Profile '{profile_name}' could not be found in shared credentials file."
        ));
    };
    let Some(session_name) = profile.get("sso_session") else {
        return Err(format!(
            "Profile '{profile_name}' is missing required property 'sso_session'."
        ));
    };
    let sessions = load_sso_sessions(input.env);
    let Some(session) = sessions.get(session_name) else {
        return Err(format!(
            "Sso session '{session_name}' could not be found in shared credentials file."
        ));
    };
    for key in ["sso_start_url", "sso_region"] {
        if session.get(key).is_none_or(String::is_empty) {
            return Err(format!(
                "Sso session '{session_name}' is missing required property '{key}'."
            ));
        }
    }
    Ok((session_name.clone(), session["sso_region"].clone()))
}

fn validate_sso_token_fields(
    token: &Map<String, Value>,
    keys: &[&str],
    refresh: bool,
) -> std::result::Result<(), String> {
    let suffix = if refresh { ". Cannot refresh" } else { "" };
    for key in keys {
        if !token.contains_key(*key) {
            return Err(format!(
                "Value not present for '{key}' in SSO Token{suffix}. {TOKEN_REFRESH}"
            ));
        }
    }
    Ok(())
}

fn persist_refreshed_sso_token(
    input: &ChainInput<'_>,
    session_name: &str,
    token: &Map<String, Value>,
    new_token: &str,
    expires_in: i64,
    refresh_token: Option<Value>,
) {
    let expires_at = jsrt::date::iso_string(input.now_ms + expires_in * 1000);
    let mut updated = token.clone();
    updated.insert("accessToken".into(), Value::String(new_token.to_string()));
    updated.insert("expiresAt".into(), Value::String(expires_at));
    match refresh_token {
        Some(refresh) => {
            updated.insert("refreshToken".into(), refresh);
        }
        None => {
            updated.shift_remove("refreshToken");
        }
    }
    if let Ok(text) = serde_json::to_string_pretty(&Value::Object(updated)) {
        let _ = std::fs::write(sso_token_path(session_name, input.env), text);
    }
}

fn validate_existing_sso_token(
    access_token: String,
    expiration: Option<i64>,
    now_ms: i64,
) -> std::result::Result<String, String> {
    match expiration {
        Some(ms) if ms < now_ms => Err(format!("Token is expired. {TOKEN_REFRESH}")),
        _ => Ok(access_token),
    }
}

/// `fromSso` (token-providers): the cached sso-session token, refreshed through SSO OIDC when it expires within five
/// minutes; the refreshed token is written back to the cache.
async fn sso_session_token(input: &ChainInput<'_>, profile_name: &str) -> std::result::Result<String, String> {
    let (session_name, region) = load_sso_session_region(input, profile_name)?;
    let Some(token) = read_sso_token(&session_name, input.env) else {
        return Err(format!(
            "The SSO session token associated with profile={profile_name} was not found or is invalid. {TOKEN_REFRESH}"
        ));
    };
    validate_sso_token_fields(&token, &["accessToken", "expiresAt"], false)?;
    let access_token = js_string(&token["accessToken"]);
    let expiration = parse_js_date(&token["expiresAt"]);
    if expiration.is_some_and(|ms| ms - input.now_ms > EXPIRE_WINDOW_MS) {
        return Ok(access_token);
    }
    let last = last_refresh_attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&session_name)
        .copied()
        .unwrap_or(0);
    if input.now_ms - last < 30_000 {
        return validate_existing_sso_token(access_token, expiration, input.now_ms);
    }
    validate_sso_token_fields(&token, &["clientId", "clientSecret", "refreshToken"], true)?;
    last_refresh_attempts()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(session_name.clone(), input.now_ms);
    match refresh_sso_token(input, &region, &token).await {
        Some((new_token, expires_in, refresh_token)) => {
            persist_refreshed_sso_token(input, &session_name, &token, &new_token, expires_in, refresh_token);
            Ok(new_token)
        }
        None => validate_existing_sso_token(access_token, expiration, input.now_ms),
    }
}

/// SSO OIDC `CreateToken` with the refresh token: the new access token, its lifetime, and the new refresh token.
async fn refresh_sso_token(
    input: &ChainInput<'_>,
    region: &str,
    token: &Map<String, Value>,
) -> Option<(String, i64, Option<Value>)> {
    let endpoint = configured_endpoint(input, "SSO_OIDC").unwrap_or_else(|| regional_endpoint("oidc", region));
    let mut body = Map::new();
    body.insert("clientId".into(), token["clientId"].clone());
    body.insert("clientSecret".into(), token["clientSecret"].clone());
    body.insert("grantType".into(), Value::String("refresh_token".into()));
    body.insert("refreshToken".into(), token["refreshToken"].clone());
    let mut headers = HeaderMap::new();
    headers.insert("content-type".into(), "application/json".into());
    let reply = request(
        input,
        "POST",
        format!("{endpoint}/token"),
        headers,
        Value::Object(body).to_string(),
        None,
    )
    .await
    .ok()?;
    if !(200..300).contains(&reply.status) {
        return None;
    }
    let response = json_body(&reply.body);
    let access_token = response.get("accessToken").filter(|v| !v.is_null())?;
    let expires_in = response.get("expiresIn").and_then(Value::as_f64)?;
    let refresh = response.get("refreshToken").filter(|v| !v.is_null()).cloned();
    Some((js_string(access_token), expires_in as i64, refresh))
}

/// SHA-1 hex digest (the SSO cache file name).
pub fn sha1_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
