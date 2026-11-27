//! Shared AWS `config` and `credentials` INI parsing, profile resolution, assume-role, and `credential_process`.

use super::{ChainInput, CredentialError, Result, from_env, login, metadata, parse_js_date, sso, sts};
use crate::aws::AwsCredentials;
use crate::env::{ProviderEnv, provider_env_value};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A profile's settings in file order.
pub type Section = IndexMap<String, String>;
pub type Profiles = BTreeMap<String, Section>;

const SECTION_TYPES: [&str; 3] = ["profile", "sso-session", "services"];

/// `[\w-@+.%:/]`: the name characters of a prefixed section header.
fn is_section_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '@' | '+' | '.' | '%' | ':' | '/')
}

/// `prefixKeyRegex`: `prefix name`, the name optionally quoted.
fn prefixed_section(header: &str) -> Option<(&str, &str)> {
    let (prefix, rest) = header.split_once(char::is_whitespace)?;
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let name = match rest.chars().next() {
        Some(q @ ('"' | '\'')) if rest.len() >= 2 && rest.ends_with(q) => &rest[1..rest.len() - 1],
        _ => rest,
    };
    (!name.is_empty() && name.chars().all(is_section_name_char)).then_some((prefix, name))
}

/// The part of a line before a comment: `split(/(^|\s)[;#]/)[0]`.
fn strip_comment(line: &str) -> &str {
    let mut previous_space = true;
    for (index, c) in line.char_indices() {
        if (c == ';' || c == '#') && previous_space {
            return if index == 0 {
                ""
            } else {
                &line[..index - line[..index].chars().last().map_or(0, char::len_utf8)]
            };
        }
        previous_space = c.is_whitespace();
    }
    line
}

enum SectionHeader {
    Valid(Option<String>),
    Poisoned,
}

fn parse_section_header(line: &str) -> Option<SectionHeader> {
    if line.len() < 2 || !line.starts_with('[') || !line.ends_with(']') {
        return None;
    }
    let name = &line[1..line.len() - 1];
    if name == "__proto__" || name == "profile __proto__" {
        return Some(SectionHeader::Poisoned);
    }
    let resolved = match prefixed_section(name) {
        Some((prefix, rest)) if SECTION_TYPES.contains(&prefix) => Some(format!("{prefix}.{rest}")),
        Some(_) => None,
        None => Some(name.to_string()),
    };
    Some(SectionHeader::Valid(resolved))
}

fn insert_ini_entry(map: &mut Profiles, current: &str, sub_section: &mut Option<String>, raw: &str, line: &str) {
    let Some(equals) = line.find('=').filter(|&i| i > 0) else {
        return;
    };
    let key = line[..equals].trim();
    let value = line[equals + 1..].trim();
    if value.is_empty() {
        *sub_section = Some(key.to_string());
        return;
    }
    if sub_section.is_some() && raw.trim_start() == raw {
        *sub_section = None;
    }
    let full_key = match sub_section {
        Some(sub) => format!("{sub}.{key}"),
        None => key.to_string(),
    };
    map.entry(current.to_string())
        .or_default()
        .insert(full_key, value.to_string());
}

/// `parseIni`: sections (`prefix.name` for the known prefixes), keys, and indented sub-sections as `sub.key`. A
/// `__proto__` profile makes the whole file unreadable, as upstream's thrown error is swallowed by its caller.
pub fn parse_ini(text: &str) -> Profiles {
    let mut map = Profiles::new();
    let mut section: Option<String> = None;
    let mut sub_section: Option<String> = None;
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let line = strip_comment(raw).trim();
        if let Some(header) = parse_section_header(line) {
            match header {
                SectionHeader::Poisoned => return Profiles::new(),
                SectionHeader::Valid(next) => {
                    section = next;
                    sub_section = None;
                }
            }
            continue;
        }
        if let Some(current) = &section {
            insert_ini_entry(&mut map, current, &mut sub_section, raw, line);
        }
    }
    map
}

/// `getConfigData`: the config file's `[default]`, `[profile name]` as `name`, and other prefixed sections as is.
fn config_data(data: Profiles) -> Profiles {
    let mut out = Profiles::new();
    if let Some(default) = data.get("default") {
        out.insert("default".into(), default.clone());
    }
    for (key, value) in data {
        let Some((prefix, rest)) = key.split_once('.') else {
            continue;
        };
        if !SECTION_TYPES.contains(&prefix) {
            continue;
        }
        let name = if prefix == "profile" {
            rest.to_string()
        } else {
            key.clone()
        };
        out.insert(name, value);
    }
    out
}

pub fn home_dir(env: Option<&ProviderEnv>) -> PathBuf {
    provider_env_value("HOME", env)
        .or_else(|| provider_env_value("USERPROFILE", env))
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
        .unwrap_or_default()
}

fn config_path(env: Option<&ProviderEnv>) -> String {
    provider_env_value("AWS_CONFIG_FILE", env)
        .unwrap_or_else(|| home_dir(env).join(".aws/config").to_string_lossy().into_owned())
}

fn credentials_path(env: Option<&ProviderEnv>) -> String {
    provider_env_value("AWS_SHARED_CREDENTIALS_FILE", env)
        .unwrap_or_else(|| home_dir(env).join(".aws/credentials").to_string_lossy().into_owned())
}

fn expand_home(path: &str, env: Option<&ProviderEnv>) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home_dir(env).join(rest),
        None => PathBuf::from(path),
    }
}

fn read_ini(path: &Path) -> Profiles {
    std::fs::read_to_string(path).map_or_default(|t| parse_ini(&t))
}

/// `loadSharedConfigFiles`: the config file (through `getConfigData`) and the credentials file.
pub fn load_shared_files(env: Option<&ProviderEnv>) -> (Profiles, Profiles) {
    let config = config_data(read_ini(&expand_home(&config_path(env), env)));
    let credentials = read_ini(&expand_home(&credentials_path(env), env));
    (config, credentials)
}

/// `parseKnownFiles`: config then credentials, a credentials key replacing the config's.
pub fn parse_known_files(env: Option<&ProviderEnv>) -> Profiles {
    let (mut merged, credentials) = load_shared_files(env);
    for (name, section) in credentials {
        let entry = merged.entry(name).or_default();
        for (key, value) in section {
            entry.insert(key, value);
        }
    }
    merged
}

/// `loadSsoSessionData`: `[sso-session name]` sections of the config file (its path is not `~`-expanded).
pub(super) fn load_sso_sessions(env: Option<&ProviderEnv>) -> Profiles {
    read_ini(Path::new(&config_path(env)))
        .into_iter()
        .filter_map(|(key, value)| key.strip_prefix("sso-session.").map(|name| (name.to_string(), value)))
        .collect()
}

/// A config-file-preferred setting of `profile` (`loadConfig` with `preferredFile: "config"`).
pub fn profile_config_value(profile: &str, key: &str, env: Option<&ProviderEnv>) -> Option<String> {
    let (config, credentials) = load_shared_files(env);
    config
        .get(profile)
        .and_then(|s| s.get(key))
        .or_else(|| credentials.get(profile).and_then(|s| s.get(key)))
        .cloned()
}

fn has(section: Option<&Section>, key: &str) -> bool {
    section.is_some_and(|s| s.contains_key(key))
}

fn is_static_profile(section: Option<&Section>) -> bool {
    has(section, "aws_access_key_id") && has(section, "aws_secret_access_key")
}

fn is_assume_role_profile(section: Option<&Section>) -> bool {
    has(section, "role_arn")
        && ((has(section, "source_profile") && !has(section, "credential_source"))
            || (has(section, "credential_source") && !has(section, "source_profile")))
}

fn is_credential_source_without_role_arn(section: Option<&Section>) -> bool {
    !has(section, "role_arn")
        && section
            .and_then(|s| s.get("credential_source"))
            .is_some_and(|v| !v.is_empty())
}

pub(super) fn is_sso_profile(section: Option<&Section>) -> bool {
    [
        "sso_start_url",
        "sso_account_id",
        "sso_session",
        "sso_region",
        "sso_role_name",
    ]
    .iter()
    .any(|key| has(section, key))
}

fn static_credentials(section: &Section) -> AwsCredentials {
    AwsCredentials {
        access_key_id: section["aws_access_key_id"].clone(),
        secret_access_key: section["aws_secret_access_key"].clone(),
        session_token: section.get("aws_session_token").cloned(),
    }
}

pub(super) async fn from_ini(input: &ChainInput<'_>, profile: &str) -> Result<AwsCredentials> {
    let profiles = parse_known_files(input.env);
    resolve_profile_data(input, profile, &profiles, &mut Vec::new(), false).await
}

async fn resolve_non_assume_profile(
    input: &ChainInput<'_>,
    name: &str,
    data: Option<&Section>,
) -> Result<AwsCredentials> {
    if let Some(section) = data.filter(|s| is_static_profile(Some(s))) {
        return Ok(static_credentials(section));
    }
    if let Some(section) = data.filter(|s| s.contains_key("web_identity_token_file") && s.contains_key("role_arn")) {
        let config = sts::WebIdentity {
            token_file: section.get("web_identity_token_file").cloned(),
            role_arn: section.get("role_arn").cloned(),
            session_name: section.get("role_session_name").cloned(),
        };
        return sts::from_token_file(input, Some(config)).await;
    }
    if has(data, "credential_process") {
        return from_process(input, name).await;
    }
    if is_sso_profile(data) {
        return sso::from_sso(input, name).await;
    }
    if let Some(section) = data.filter(|s| has(Some(s), "login_session")) {
        return login::from_login(input, section).await;
    }
    Err(CredentialError::next(format!(
        "Could not resolve credentials using profile: [{name}] in configuration/credentials file(s)."
    )))
}

fn resolve_profile_data<'a>(
    input: &'a ChainInput<'a>,
    name: &'a str,
    profiles: &'a Profiles,
    visited: &'a mut Vec<String>,
    assume_role_recursive: bool,
) -> crate::http::BoxFuture<'a, Result<AwsCredentials>> {
    Box::pin(async move {
        let data = profiles.get(name);
        if !visited.is_empty() && is_static_profile(data) {
            return Ok(static_credentials(data.unwrap_or(&Section::new())));
        }
        if assume_role_recursive || is_assume_role_profile(data) {
            return resolve_assume_role(input, name, profiles, visited).await;
        }
        resolve_non_assume_profile(input, name, data).await
    })
}

fn validate_assume_role(
    input: &ChainInput<'_>,
    name: &str,
    data: &Section,
    visited: &[String],
    without_role: bool,
) -> Result<()> {
    if let Some(source) = data.get("source_profile")
        && visited.contains(source)
    {
        return Err(CredentialError::next(format!(
            "Detected a cycle attempting to resolve credentials for profile {}. Profiles visited: {}",
            input.profile_name(),
            visited.join(", ")
        )));
    }
    if !without_role && data.get("mfa_serial").is_some_and(|s| !s.is_empty()) {
        return Err(CredentialError::stop(format!(
            "Profile {name} requires multi-factor authentication, but no MFA code callback was provided."
        )));
    }
    Ok(())
}

fn assume_role_params(input: &ChainInput<'_>, data: &Section) -> Vec<(&'static str, String)> {
    let mut params = vec![
        ("RoleArn", data.get("role_arn").cloned().unwrap_or_default()),
        (
            "RoleSessionName",
            data.get("role_session_name")
                .filter(|s| !s.is_empty())
                .cloned()
                .unwrap_or_else(|| format!("aws-sdk-js-{}", input.now_ms)),
        ),
        (
            "DurationSeconds",
            parse_int_prefix(data.get("duration_seconds").map_or("3600", String::as_str))
                .map_or_else(|| "NaN".into(), |n| n.to_string()),
        ),
    ];
    if let Some(external) = data.get("external_id") {
        params.push(("ExternalId", external.clone()));
    }
    params
}

async fn resolve_assume_role(
    input: &ChainInput<'_>,
    name: &str,
    profiles: &Profiles,
    visited: &mut Vec<String>,
) -> Result<AwsCredentials> {
    let empty = Section::new();
    let data = profiles.get(name).unwrap_or(&empty);
    let without_role = is_credential_source_without_role_arn(Some(data));
    validate_assume_role(input, name, data, visited, without_role)?;
    let source = match data.get("source_profile") {
        Some(source) => {
            visited.push(source.clone());
            let recursive = is_credential_source_without_role_arn(Some(profiles.get(source).unwrap_or(&empty)));
            resolve_profile_data(input, source, profiles, visited, recursive).await?
        }
        None => {
            let credential_source = data.get("credential_source").cloned().unwrap_or_default();
            credential_source_credentials(input, &credential_source, name).await?
        }
    };
    if without_role {
        return Ok(source);
    }
    let region = data.get("region").cloned().unwrap_or_else(|| input.region.to_string());
    let params = assume_role_params(input, data);
    let role_arn = params[0].1.clone();
    sts::sts_call(
        input,
        &region,
        "AssumeRole",
        &params,
        Some(&source),
        &role_arn,
        "assumeRole",
    )
    .await
}

/// `parseInt(text, 10)`.
fn parse_int_prefix(text: &str) -> Option<i64> {
    let trimmed = text.trim_start();
    let (sign, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}

/// `resolveCredentialSource`.
async fn credential_source_credentials(input: &ChainInput<'_>, source: &str, profile: &str) -> Result<AwsCredentials> {
    match source {
        "EcsContainer" => metadata::container_credentials(input).await,
        "Ec2InstanceMetadata" => metadata::from_instance_metadata(input).await,
        "Environment" => from_env(input),
        other => Err(CredentialError::next(format!(
            "Unsupported credential source in profile {profile}. Got {other}, expected EcsContainer or Ec2InstanceMetadata or Environment."
        ))),
    }
}

async fn run_credential_process(input: &ChainInput<'_>, command: &str) -> Result<String> {
    let overlay: Vec<(String, String)> = input
        .env
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let shell = command.to_string();
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&shell)
            .envs(overlay)
            .stdin(std::process::Stdio::null())
            .output()
    })
    .await
    .map_err(|e| CredentialError::next(e.to_string()))?
    .map_err(|e| CredentialError::next(e.to_string()))?;
    if !output.status.success() {
        return Err(CredentialError::next(format!(
            "Command failed: {command}\n{}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_process_credentials(input: &ChainInput<'_>, profile: &str, stdout: &str) -> Result<AwsCredentials> {
    let data: Value = serde_json::from_str(stdout.trim())
        .map_err(|_| CredentialError::next(format!("Profile {profile} credential_process returned invalid JSON.")))?;
    if data.get("Version").and_then(Value::as_f64) != Some(1.0) {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process did not return Version 1."
        )));
    }
    let text = |key: &str| match data.get(key) {
        None => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    };
    let (Some(access_key_id), Some(secret_access_key)) = (text("AccessKeyId"), text("SecretAccessKey")) else {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process returned invalid credentials."
        )));
    };
    if let Some(expiration) = data.get("Expiration").and_then(Value::as_str).filter(|e| !e.is_empty())
        && jsrt::date::parse_date(expiration).is_some_and(|ms| ms < input.now_ms)
    {
        return Err(CredentialError::next(format!(
            "Profile {profile} credential_process returned expired credentials."
        )));
    }
    input.expiration.set(
        data.get("Expiration")
            .filter(|e| !e.is_null() && *e != "" && *e != false && *e != 0)
            .and_then(parse_js_date),
    );
    Ok(AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token: data
            .get("SessionToken")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// `fromProcess`: the profile's `credential_process` run by the shell, its JSON validated.
pub(super) async fn from_process(input: &ChainInput<'_>, profile: &str) -> Result<AwsCredentials> {
    let profiles = parse_known_files(input.env);
    let Some(section) = profiles.get(profile) else {
        return Err(CredentialError::next(format!(
            "Profile {profile} could not be found in shared credentials file."
        )));
    };
    let Some(command) = section.get("credential_process").cloned() else {
        return Err(CredentialError::next(format!(
            "Profile {profile} did not contain credential_process."
        )));
    };
    let stdout = run_credential_process(input, &command).await?;
    parse_process_credentials(input, profile, &stdout)
}
