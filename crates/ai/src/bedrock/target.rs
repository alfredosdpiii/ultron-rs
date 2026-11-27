//! Region, endpoint, credential chain resolution, and SigV4 request construction for Amazon Bedrock.

use super::BedrockOptions;
use super::error::BedrockError;
use crate::aws::{AwsCredentials, SignableRequest, escape_uri, sign_request};
use crate::aws_credentials::{
    ChainInput, CredentialError, Profiles, Section, default_credentials, load_shared_files, profile_config_value,
};
use crate::env::{ProviderEnv, provider_env_value};
use crate::http::{HttpRequest, Transport};
use crate::types::Model;

/// Credentials that expire within this long count as expired (`credentialsTreatedAsExpired`).
const TREATED_AS_EXPIRED_MS: i64 = 300_000;

pub(super) enum Auth {
    Bearer(String),
    SigV4(AwsCredentials),
    /// Resolved through the SDK default chain (for this profile) when the request is sent.
    Chain(Option<String>),
}

pub(super) struct Target {
    pub(super) endpoint: String,
    pub(super) region: String,
    pub(super) auth: Auth,
}

struct RegionInput<'a> {
    model: &'a Model,
    configured: Option<String>,
    endpoint_region: Option<String>,
    explicit_endpoint: bool,
    ambient_profile: bool,
    profile: Option<&'a str>,
    env: Option<&'a ProviderEnv>,
}

pub(super) fn configured_region(options: &BedrockOptions) -> Option<String> {
    options
        .region
        .clone()
        .filter(|r| !r.is_empty())
        .or_else(|| provider_env_value("AWS_REGION", options.base.env.as_ref()))
        .or_else(|| provider_env_value("AWS_DEFAULT_REGION", options.base.env.as_ref()))
}

fn is_region_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
}

fn is_valid_aws_region(region: &str) -> bool {
    !region.is_empty() && region.chars().all(is_region_char)
}

fn validated_region(region: &str) -> Option<String> {
    if is_valid_aws_region(region) {
        Some(region.to_string())
    } else {
        None
    }
}

fn standard_endpoint_region(base_url: &str) -> Option<String> {
    let host = reqwest::Url::parse(base_url).ok()?.host_str()?.to_lowercase();
    let rest = host
        .strip_prefix("bedrock-runtime-fips.")
        .or(host.strip_prefix("bedrock-runtime."))?;
    let region = rest
        .strip_suffix(".amazonaws.com")
        .or(rest.strip_suffix(".amazonaws.com.cn"))?;
    validated_region(region)
}

fn strip_arn_partition(rest: &str) -> Option<&str> {
    match rest.strip_prefix('-') {
        Some(partition) => Some(partition.split_once(':')?.1),
        None => rest.strip_prefix(':'),
    }
}

fn arn_region(model_id: &str) -> Option<String> {
    let rest = model_id.strip_prefix("arn:aws")?;
    let rest = strip_arn_partition(rest)?;
    let rest = rest.strip_prefix("bedrock:")?;
    let (region, _) = rest.split_once(':')?;
    validated_region(region)
}

fn non_empty_string_ref(value: Option<&String>) -> Option<String> {
    value.filter(|s| !s.is_empty()).cloned()
}

fn env_map_value(env: Option<&ProviderEnv>, key: &str) -> Option<String> {
    let value = env?.get(key)?;
    if value.is_empty() { None } else { Some(value.clone()) }
}

fn resolve_profile(options: &BedrockOptions) -> (Option<String>, Option<String>) {
    let env = options.base.env.as_ref();
    let options_profile = if let Some(profile) = non_empty_string_ref(options.profile.as_ref()) {
        Some(profile)
    } else {
        env_map_value(env, "AWS_PROFILE")
    };
    let profile = match &options_profile {
        Some(p) => Some(p.clone()),
        None => provider_env_value("AWS_PROFILE", env),
    };
    (options_profile, profile)
}

fn resolve_region(input: RegionInput<'_>) -> Result<String, BedrockError> {
    if let Some(region) = arn_region(&input.model.id) {
        return Ok(region);
    }
    if let Some(region) = input.configured {
        return Ok(region);
    }
    if input.explicit_endpoint
        && let Some(region) = input.endpoint_region
    {
        return Ok(region);
    }
    if !input.ambient_profile {
        return Ok("us-east-1".to_string());
    }
    if let Some(region) = profile_config_value(input.profile.unwrap_or("default"), "region", input.env) {
        return Ok(region);
    }
    Err(BedrockError::plain("Region is missing"))
}

fn lookup_process_env(name: &str) -> Option<String> {
    provider_env_value(name, None)
}

fn resolve_endpoint(model: &Model, region: &str, explicit_endpoint: bool, env: Option<&ProviderEnv>) -> String {
    if explicit_endpoint {
        return model.base_url.trim_end_matches('/').to_string();
    }
    let configured_endpoint = configured_endpoint_url(&lookup_process_env, &|| load_shared_files(env));
    if let Some(url) = configured_endpoint {
        return url.trim_end_matches('/').to_string();
    }
    if region.starts_with("cn-") {
        format!("https://bedrock-runtime.{region}.amazonaws.com.cn")
    } else {
        format!("https://bedrock-runtime.{region}.amazonaws.com")
    }
}

fn bearer_token(options: &BedrockOptions) -> Option<String> {
    if let Some(token) = non_empty_string_ref(options.bearer_token.as_ref()) {
        return Some(token);
    }
    if let Some(token) = non_empty_string_ref(options.base.api_key.as_ref()) {
        return Some(token);
    }
    provider_env_value("AWS_BEARER_TOKEN_BEDROCK", options.base.env.as_ref())
}

fn resolve_auth(options: &BedrockOptions, options_profile: Option<&str>, profile: Option<String>) -> Auth {
    let env = options.base.env.as_ref();
    let skip_auth = provider_env_value("AWS_BEDROCK_SKIP_AUTH", env).as_deref() == Some("1");
    if skip_auth {
        return Auth::SigV4(AwsCredentials {
            access_key_id: "dummy-access-key".into(),
            secret_access_key: "dummy-secret-key".into(),
            session_token: None,
        });
    }
    if let Some(token) = bearer_token(options) {
        return Auth::Bearer(token);
    }
    if options_profile.is_none()
        && let Some(credentials) = env_credentials(env)
    {
        return Auth::SigV4(credentials);
    }
    Auth::Chain(profile)
}

pub(super) fn resolve_target(model: &Model, options: &BedrockOptions) -> Result<Target, BedrockError> {
    let env = options.base.env.as_ref();
    let (options_profile, profile) = resolve_profile(options);
    let configured = configured_region(options);
    let ambient_profile = provider_env_value("AWS_PROFILE", None).is_some();
    let endpoint_region = standard_endpoint_region(&model.base_url);
    let explicit_endpoint = endpoint_region.is_none() || (configured.is_none() && !ambient_profile);
    let region = resolve_region(RegionInput {
        model,
        configured,
        endpoint_region,
        explicit_endpoint,
        ambient_profile,
        profile: profile.as_deref(),
        env,
    })?;
    let endpoint = resolve_endpoint(model, &region, explicit_endpoint, env);
    let auth = resolve_auth(options, options_profile.as_deref(), profile);
    Ok(Target { endpoint, region, auth })
}

fn default_profile_name() -> String {
    "default".into()
}

fn merged_profile_config(config: &Profiles, credentials: &Profiles, name: &str) -> Section {
    let mut profile = credentials.get(name).cloned().unwrap_or_default();
    for (key, value) in config.get(name).cloned().unwrap_or_default() {
        profile.insert(key, value);
    }
    profile
}

fn parse_sdk_boolean(value: Option<String>) -> Option<bool> {
    match value.as_deref() {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    }
}

fn non_empty_string(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

fn should_ignore_configured_endpoints(lookup: &dyn Fn(&str) -> Option<String>, profile: &Section) -> bool {
    if let Some(ignore) = parse_sdk_boolean(lookup("AWS_IGNORE_CONFIGURED_ENDPOINT_URLS")) {
        return ignore;
    }
    parse_sdk_boolean(profile.get("ignore_configured_endpoint_urls").cloned()).unwrap_or(false)
}

fn first_env_endpoint(lookup: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(url) = non_empty_string(lookup("AWS_ENDPOINT_URL_BEDROCK_RUNTIME")) {
        return Some(url);
    }
    non_empty_string(lookup("AWS_ENDPOINT_URL"))
}

/// The SDK's configured endpoint (`getEndpointFromConfig("Bedrock Runtime")`, used when the client sets no
/// `endpoint`): none when `AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` (else the profile's `ignore_configured_endpoint_urls`)
/// is `true`; else `AWS_ENDPOINT_URL_BEDROCK_RUNTIME`, then `AWS_ENDPOINT_URL`, then the profile's `services`
/// section's `bedrock_runtime.endpoint_url`, then the profile's `endpoint_url`. `lookup` is the process environment
/// (`loadConfig` reads `process.env`); the profile is `AWS_PROFILE` or `default`, config file over credentials file.
/// A boolean that is neither `true` nor `false` is skipped, as `loadConfig` moves on to the next source.
pub fn configured_endpoint_url(
    lookup: &dyn Fn(&str) -> Option<String>,
    files: &dyn Fn() -> (Profiles, Profiles),
) -> Option<String> {
    let (config, credentials) = files();
    let name = lookup("AWS_PROFILE").unwrap_or_else(default_profile_name);
    let profile = merged_profile_config(&config, &credentials, &name);
    if should_ignore_configured_endpoints(lookup, &profile) {
        return None;
    }
    if let Some(url) = first_env_endpoint(lookup) {
        return Some(url);
    }
    if let Some(services) = profile.get("services") {
        let section = config.get(&format!("services.{services}"))?;
        if let Some(url) = non_empty_string(section.get("bedrock_runtime.endpoint_url").cloned()) {
            return Some(url);
        }
    }
    non_empty_string(profile.get("endpoint_url").cloned())
}

fn env_credentials(env: Option<&ProviderEnv>) -> Option<AwsCredentials> {
    Some(AwsCredentials {
        access_key_id: provider_env_value("AWS_ACCESS_KEY_ID", env)?,
        secret_access_key: provider_env_value("AWS_SECRET_ACCESS_KEY", env)?,
        session_token: provider_env_value("AWS_SESSION_TOKEN", env),
    })
}

/// What the client's second credential lookup does with the first lookup's credentials (`memoizeChain`): the
/// auth scheme resolves the identity, then the signer asks the same memoized chain again.
#[derive(Debug, PartialEq, Eq)]
pub enum SecondLookup {
    /// No expiration, or more than five minutes left: the cached credentials sign.
    Cached,
    /// Expiring within five minutes: they sign, and the chain runs again in the background (`passiveLock`).
    RefreshInBackground,
    /// Already expired: the cache is dropped and the chain runs again; its credentials sign.
    LookUpAgain,
}

/// [`SecondLookup`] for credentials expiring at `expiration` (ms), looked up again at `now`.
pub fn second_lookup(expiration: Option<i64>, now: i64) -> SecondLookup {
    match expiration {
        Some(at) if at < now => SecondLookup::LookUpAgain,
        Some(at) if at - now < TREATED_AS_EXPIRED_MS => SecondLookup::RefreshInBackground,
        _ => SecondLookup::Cached,
    }
}

async fn run_chain(
    region: &str,
    profile: Option<&str>,
    env: Option<&ProviderEnv>,
    transport: &dyn Transport,
) -> Result<(AwsCredentials, Option<i64>), CredentialError> {
    let input = ChainInput {
        env,
        transport,
        profile,
        region,
        now_ms: jsrt::date::now_ms(),
        expiration: Default::default(),
    };
    let credentials = default_credentials(&input).await?;
    Ok((credentials, input.expiration.get()))
}

fn from_credential_error(error: CredentialError) -> BedrockError {
    BedrockError {
        name: error.name,
        message: error.message,
        status: error.status,
        ..Default::default()
    }
}

fn spawn_background_refresh(
    region: String,
    profile: Option<String>,
    env: Option<ProviderEnv>,
    detached: std::sync::Arc<dyn Transport>,
) {
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
            return;
        };
        let _ = runtime.block_on(run_chain(&region, profile.as_deref(), env.as_ref(), &*detached));
    });
}

fn maybe_spawn_background_refresh(
    region: &str,
    profile: Option<&str>,
    env: Option<&ProviderEnv>,
    transport: &dyn Transport,
) {
    let Some(detached) = transport.detached() else {
        return;
    };
    spawn_background_refresh(region.to_string(), profile.map(str::to_string), env.cloned(), detached);
}

/// The SDK default chain for the client's profile and region, memoized for the call as the client's
/// `defaultProvider` is (see [`SecondLookup`]). The background refresh runs on its own thread over a detached copy
/// of the transport and its outcome is dropped with the client, as the SDK's is; a transport that cannot detach
/// skips it.
pub(super) async fn chain_credentials(
    target: &Target,
    profile: Option<&str>,
    options: &BedrockOptions,
    transport: &dyn Transport,
) -> Result<AwsCredentials, BedrockError> {
    let env = options.base.env.as_ref();
    let (credentials, expiration) = run_chain(&target.region, profile, env, transport)
        .await
        .map_err(from_credential_error)?;
    match second_lookup(expiration, jsrt::date::now_ms()) {
        SecondLookup::Cached => Ok(credentials),
        SecondLookup::LookUpAgain => Ok(run_chain(&target.region, profile, env, transport)
            .await
            .map_err(from_credential_error)?
            .0),
        SecondLookup::RefreshInBackground => {
            maybe_spawn_background_refresh(&target.region, profile, env, transport);
            Ok(credentials)
        }
    }
}

fn is_reserved_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("x-amz-") || lower == "authorization" || lower == "host"
}

fn endpoint_host(endpoint: &str) -> String {
    reqwest::Url::parse(endpoint)
        .ok()
        .and_then(|u| {
            u.host_str().map(|h| match u.port() {
                Some(port) => format!("{h}:{port}"),
                None => h.to_string(),
            })
        })
        .unwrap_or_default()
}

fn sign_bedrock_request(
    headers: Vec<(String, String)>,
    path: String,
    host: String,
    body: &str,
    credentials: &AwsCredentials,
    region: &str,
) -> Vec<(String, String)> {
    let mut all = headers;
    all.push(("host".into(), host));
    let mut signable = SignableRequest {
        method: "POST".into(),
        path,
        query: Vec::new(),
        headers: all,
        body: body.as_bytes().to_vec(),
    };
    sign_request(&mut signable, credentials, region, "bedrock", jsrt::date::now_ms());
    signable.headers
}

pub(super) fn build_request(target: &Target, model: &Model, body: &str, options: &BedrockOptions) -> HttpRequest {
    let path_prefix =
        reqwest::Url::parse(&target.endpoint).map_or_default(|u| u.path().trim_end_matches('/').to_string());
    let path = format!("{path_prefix}/model/{}/converse-stream", escape_uri(&model.id));
    let host = endpoint_host(&target.endpoint);
    let origin = target.endpoint.strip_suffix(&path_prefix).unwrap_or(&target.endpoint);
    let mut headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    for (name, value) in options.base.headers.iter().flatten() {
        if let Some(value) = value
            && !is_reserved_header(name)
        {
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
            headers.push((name.to_ascii_lowercase(), value.clone()));
        }
    }
    match &target.auth {
        Auth::Bearer(token) => headers.push(("authorization".into(), format!("Bearer {token}"))),
        Auth::Chain(_) => {}
        Auth::SigV4(credentials) => {
            headers = sign_bedrock_request(headers, path.clone(), host, body, credentials, &target.region);
        }
    }
    HttpRequest {
        method: "POST".into(),
        url: format!("{origin}{path}"),
        headers: headers.into_iter().collect(),
        body: body.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn configured_endpoint_url_short_circuits_lookup_callbacks() {
        let queried = RefCell::new(Vec::new());
        let lookup = |name: &str| -> Option<String> {
            queried.borrow_mut().push(name.to_string());
            match name {
                "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS" => Some("false".to_string()),
                "AWS_ENDPOINT_URL_BEDROCK_RUNTIME" => Some("https://bedrock.example.test".to_string()),
                "AWS_ENDPOINT_URL" => panic!("fallback endpoint key should not be queried"),
                _ => None,
            }
        };
        let files = || (Profiles::new(), Profiles::new());

        let url = configured_endpoint_url(&lookup, &files);

        assert_eq!(url.as_deref(), Some("https://bedrock.example.test"));
        assert!(!queried.borrow().iter().any(|k| k == "AWS_ENDPOINT_URL"));
    }

    #[test]
    fn should_ignore_configured_endpoints_short_circuits_on_env_value() {
        let mut profile = Section::new();
        profile.insert("ignore_configured_endpoint_urls".into(), "true".into());
        let lookup = |name: &str| -> Option<String> {
            (name == "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS").then(|| "false".to_string())
        };

        assert!(!should_ignore_configured_endpoints(&lookup, &profile));
    }

    #[test]
    fn bearer_token_and_resolve_profile_prefer_higher_precedence_values() {
        let mut env = ProviderEnv::new();
        env.insert("AWS_BEARER_TOKEN_BEDROCK".into(), "env-token".into());
        env.insert("AWS_PROFILE".into(), "env-profile".into());

        let options = BedrockOptions {
            base: crate::options::StreamOptions {
                api_key: Some("api-key".into()),
                env: Some(env),
                ..Default::default()
            },
            bearer_token: Some("opt-token".into()),
            profile: Some("opt-profile".into()),
            ..Default::default()
        };

        assert_eq!(bearer_token(&options).as_deref(), Some("opt-token"));
        assert_eq!(
            resolve_profile(&options),
            (Some("opt-profile".to_string()), Some("opt-profile".to_string()))
        );
    }
}
