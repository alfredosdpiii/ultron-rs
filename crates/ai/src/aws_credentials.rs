//! The AWS SDK v3 default credential chain the Bedrock provider relies on (`@aws-sdk/credential-provider-node`'s
//! `defaultProvider` and its sub-providers), without the SDK: environment keys, the shared `config`/`credentials`
//! files as `@smithy/core/config` parses them, profile static keys, assume role (source profile or credential
//! source), web identity (env and profile), `credential_process`, SSO (sso-session with token refresh, and legacy),
//! `aws login` sessions (`login_session`: the cached token and its DPoP-signed Signin refresh), container credentials
//! (`fromHttp`, `fromContainerMetadata`) and EC2 instance metadata (IMDSv2 with the v1 fallback). Each link fails
//! with the SDK's message; a failure that does not allow the next link ends the chain, as upstream.
//!
//! Settings the SDK reads from `process.env` are read through `provider_env_value` (the provider env, then the
//! process env), so a stored credential's env or a test can scope them. Every request goes through the provider's
//! `Transport`.

mod ini;
mod login;
mod metadata;
mod sso;
mod sts;

pub use ini::{Profiles, Section, home_dir, load_shared_files, parse_ini, parse_known_files, profile_config_value};
pub use login::{parse_sec1_pem, signin_endpoint};
pub use sso::{sha1_hex, sso_token_path};

use crate::aws::AwsCredentials;
use crate::env::{ProviderEnv, provider_env_value};
use crate::http::{HeaderMap, HttpRequest, Transport};
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;

/// A failed link: `name` and `message` as upstream's error, and whether the chain moves on (`tryNextLink`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialError {
    pub name: String,
    pub message: String,
    pub try_next: bool,
    /// `$metadata.httpStatusCode` of a service error (STS).
    pub status: Option<u16>,
}

impl CredentialError {
    /// A `CredentialsProviderError` that lets the chain continue (upstream's default).
    fn next(message: impl Into<String>) -> Self {
        Self::provider(message, true)
    }

    /// A `CredentialsProviderError` that ends the chain.
    fn stop(message: impl Into<String>) -> Self {
        Self::provider(message, false)
    }

    fn provider(message: impl Into<String>, try_next: bool) -> Self {
        CredentialError {
            name: "CredentialsProviderError".into(),
            message: message.into(),
            try_next,
            status: None,
        }
    }

    /// A plain `Error` (or a `TypeError`): not a provider error, so it ends the chain.
    fn plain(name: &str, message: impl Into<String>) -> Self {
        CredentialError {
            name: name.into(),
            message: message.into(),
            try_next: false,
            status: None,
        }
    }
}

type Result<T> = std::result::Result<T, CredentialError>;

/// What the chain needs from the calling client.
pub struct ChainInput<'a> {
    pub env: Option<&'a ProviderEnv>,
    pub transport: &'a dyn Transport,
    /// The client's `profile` (`init.profile`); `None` falls back to `AWS_PROFILE`, then `default`.
    pub profile: Option<&'a str>,
    /// The calling client's region: STS uses it unless the profile names its own.
    pub region: &'a str,
    pub now_ms: i64,
    /// Set by the chain: the returned credentials' `expiration`.
    pub expiration: Expiration,
}

/// The `expiration` (ms since the epoch) of the credentials a link returned, as the SDK's `credentials.expiration`;
/// `None` when the link reports none (static keys) or the date is invalid (a `NaN` time never counts as expiring).
#[derive(Debug, Default)]
pub struct Expiration(Mutex<Option<i64>>);

impl Expiration {
    pub fn get(&self) -> Option<i64> {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, ms: Option<i64>) {
        *self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = ms;
    }
}

impl ChainInput<'_> {
    fn var(&self, name: &str) -> Option<String> {
        provider_env_value(name, self.env)
    }

    fn profile_name(&self) -> String {
        self.profile
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .or_else(|| self.var("AWS_PROFILE"))
            .unwrap_or_else(|| "default".into())
    }
}

/// `defaultProvider(init)()`.
pub async fn default_credentials(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    input.expiration.set(None);
    let links: [Link; 5] = [Link::Env, Link::Ini, Link::Process, Link::TokenFile, Link::Remote];
    for link in links {
        let attempt = match link {
            Link::Env => from_env_link(input),
            Link::Ini => ini::from_ini(input, &input.profile_name()).await,
            Link::Process => ini::from_process(input, &input.profile_name()).await,
            Link::TokenFile => sts::from_token_file(input, None).await,
            Link::Remote => metadata::remote_provider(input).await,
        };
        match attempt {
            Ok(credentials) => return Ok(credentials),
            Err(error) if error.try_next => continue,
            Err(error) => return Err(error),
        }
    }
    Err(CredentialError::stop("Could not load credentials from any providers"))
}

#[derive(Clone, Copy)]
enum Link {
    Env,
    Ini,
    Process,
    TokenFile,
    Remote,
}

fn from_env_link(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    if input.profile.is_some_and(|p| !p.is_empty()) || input.var("AWS_PROFILE").is_some() {
        return Err(CredentialError::next("AWS_PROFILE is set, skipping fromEnv provider."));
    }
    from_env(input)
}

fn from_env(input: &ChainInput<'_>) -> Result<AwsCredentials> {
    match (input.var("AWS_ACCESS_KEY_ID"), input.var("AWS_SECRET_ACCESS_KEY")) {
        (Some(access_key_id), Some(secret_access_key)) => {
            let expiry = input.var("AWS_CREDENTIAL_EXPIRATION").filter(|e| !e.is_empty());
            input.expiration.set(expiry.as_deref().and_then(jsrt::date::parse_date));
            Ok(AwsCredentials {
                access_key_id,
                secret_access_key,
                session_token: input.var("AWS_SESSION_TOKEN"),
            })
        }
        _ => Err(CredentialError::next(
            "Unable to find environment variable credentials.",
        )),
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: String,
}

async fn request(
    input: &ChainInput<'_>,
    method: &str,
    url: String,
    headers: HeaderMap,
    body: String,
    timeout: Option<Duration>,
) -> std::result::Result<Reply, String> {
    let exchange = async {
        let mut response = input
            .transport
            .send(HttpRequest {
                method: method.into(),
                url,
                headers,
                body,
            })
            .await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.body.next_chunk().await? {
            bytes.extend_from_slice(&chunk);
        }
        Ok(Reply {
            status: response.status,
            headers: response.headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        })
    };
    match timeout {
        Some(limit) => tokio::time::timeout(limit, exchange)
            .await
            .unwrap_or_else(|_| Err("TimeoutError".into())),
        None => exchange.await,
    }
}

/// `AWS_ENDPOINT_URL_<SERVICE>`, then `AWS_ENDPOINT_URL`.
fn configured_endpoint(input: &ChainInput<'_>, service: &str) -> Option<String> {
    input
        .var(&format!("AWS_ENDPOINT_URL_{service}"))
        .or_else(|| input.var("AWS_ENDPOINT_URL"))
        .map(|url| url.trim_end_matches('/').to_string())
}

fn regional_endpoint(prefix: &str, region: &str) -> String {
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    format!("https://{prefix}.{region}.{suffix}")
}

fn json_body(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or(Value::Null)
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_js_date(value: &Value) -> Option<i64> {
    match value {
        Value::String(s) => jsrt::date::parse_date(s),
        Value::Number(n) => n.as_f64().map(|f| f as i64),
        _ => None,
    }
}
