//! Provider auth of `packages/ai` (`auth/types.ts`, `auth/resolve.ts`, `auth/helpers.ts`, `auth/credential-store.ts`,
//! `auth/context.ts`): credentials (api-key and OAuth, the `auth.json` entry shapes), login interaction, credential
//! stores (in memory with per-provider serialized writes, or `auth.json` with the TypeScript product's lock and
//! format), the built-in providers' handlers (Anthropic, OpenAI, OpenAI Codex, GitHub Copilot, Google, Google Vertex,
//! OpenRouter), `resolveProviderAuth` with its locked OAuth refresh, and `mergeHeaders`.
//!
//! Evidence: `fixtures/generate-auth.mjs` (TypeScript `resolveProviderAuth` over the built-in providers,
//! `tests/auth.rs`) and `fixtures/generate-rest.mjs auth` (the `auth.json` store, `tests/auth_rest.rs`); the store's
//! locking and cancellation rules are ported from `models-runtime.test.ts`.

use crate::http::{AbortSignal, BoxFuture, Transport, is_aborted};
use crate::oauth::OAuthProvider;
use crate::oauth_flow::{Clock, SystemLoginEnv};
use crate::oauth_login::LoginContext;
use crate::options::ProviderHeaders;
use indexmap::IndexMap;
use jsrt::date::now_ms;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// Provider-scoped config values (Vertex project/location, Cloudflare account/gateway ids, ...), in insertion order.
pub type CredentialEnv = IndexMap<String, String>;

// ------------------------------------------------------------------------------------------------------ credentials

/// Request auth for one model request (`ModelAuth`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelAuth {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<ProviderHeaders>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

/// Stored api-key credential; `env` holds provider-scoped config.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ApiKeyCredential {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<CredentialEnv>,
}

/// OAuth token data; provider-specific fields (`accountId`, `enterpriseUrl`, `availableModelIds`, `scope`, ...) ride
/// along in `extra`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub refresh: String,
    pub access: String,
    pub expires: f64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl OAuthCredential {
    /// Reads the `auth.json` entry shape (with or without `"type"`); `None` when a required field is missing or
    /// mistyped.
    pub fn from_json(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        let mut extra = map.clone();
        for key in ["type", "refresh", "access", "expires"] {
            extra.shift_remove(key);
        }
        Some(OAuthCredential {
            refresh: map.get("refresh")?.as_str()?.to_owned(),
            access: map.get("access")?.as_str()?.to_owned(),
            expires: map.get("expires")?.as_f64()?,
            extra,
        })
    }

    /// The `auth.json` entry: `{"type":"oauth", refresh, access, expires, ...extra}`, integral numbers as integers.
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("type".into(), json!("oauth"));
        map.insert("refresh".into(), json!(self.refresh));
        map.insert("access".into(), json!(self.access));
        map.insert("expires".into(), json!(self.expires));
        map.extend(self.extra.clone());
        jsrt::json::normalize_numbers(Value::Object(map))
    }
}

/// One type-tagged credential per provider: the shape of `auth.json` entries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Credential {
    #[serde(rename = "api_key")]
    ApiKey(ApiKeyCredential),
    #[serde(rename = "oauth")]
    OAuth(OAuthCredential),
}

impl Credential {
    /// Reads an `auth.json` entry; `None` for an unknown type or a malformed entry.
    pub fn from_json(value: &Value) -> Option<Self> {
        match value.get("type")?.as_str()? {
            "api_key" => serde_json::from_value(value.clone()).ok(),
            "oauth" => OAuthCredential::from_json(value).map(Self::OAuth),
            _ => None,
        }
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::ApiKey(_) => jsrt::json::normalize_numbers(serde_json::to_value(self).unwrap_or(Value::Null)),
            Self::OAuth(credential) => credential.to_json(),
        }
    }

    pub fn kind(&self) -> AuthType {
        match self {
            Credential::ApiKey(_) => AuthType::ApiKey,
            Credential::OAuth(_) => AuthType::OAuth,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

impl AuthType {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthType::ApiKey => "api_key",
            AuthType::OAuth => "oauth",
        }
    }
}

/// A stored credential's provider and type (`CredentialInfo`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub provider_id: String,
    #[serde(rename = "type")]
    pub kind: AuthType,
}

/// Resolved auth with provider env and a status label (`AuthResult`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthResult {
    pub auth: ModelAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<CredentialEnv>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl AuthResult {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// `checkAuth`'s answer: the source and the credential type that would be used.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuthCheck {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(rename = "type")]
    pub kind: AuthType,
}

// ------------------------------------------------------------------------------------------------------------ errors

/// `ModelsErrorCode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsErrorCode {
    ModelSource,
    ModelValidation,
    Provider,
    Stream,
    Auth,
    #[serde(rename = "oauth")]
    OAuth,
}

impl ModelsErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelSource => "model_source",
            Self::ModelValidation => "model_validation",
            Self::Provider => "provider",
            Self::Stream => "stream",
            Self::Auth => "auth",
            Self::OAuth => "oauth",
        }
    }
}

/// `ModelsError`: the message carries the cause (`withCauseDetail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelsError {
    pub code: ModelsErrorCode,
    pub message: String,
}

impl ModelsError {
    pub fn new(code: ModelsErrorCode, message: impl Into<String>) -> Self {
        ModelsError {
            code,
            message: message.into(),
        }
    }

    /// `new ModelsError(code, message, { cause })`: the cause's text is appended unless blank or already contained.
    pub fn with_cause(code: ModelsErrorCode, message: impl Into<String>, cause: &str) -> Self {
        let mut message = message.into();
        let detail = cause.trim();
        if !detail.is_empty() && !message.contains(detail) {
            message = format!("{message}: {detail}");
        }
        ModelsError { code, message }
    }
}

impl std::fmt::Display for ModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why auth resolution or a `Models` operation failed: the caller's signal aborted (`AbortError`), or a `ModelsError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthError {
    Aborted,
    Models(ModelsError),
}

impl AuthError {
    pub fn models(code: ModelsErrorCode, message: impl Into<String>, cause: Option<&str>) -> Self {
        AuthError::Models(match cause {
            Some(cause) => ModelsError::with_cause(code, message, cause),
            None => ModelsError::new(code, message),
        })
    }

    pub fn message(&self) -> String {
        match self {
            Self::Aborted => crate::provider::OPERATION_ABORTED.into(),
            Self::Models(error) => error.message.clone(),
        }
    }
}

impl From<ModelsError> for AuthError {
    fn from(error: ModelsError) -> Self {
        AuthError::Models(error)
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

// --------------------------------------------------------------------------------------------- interaction / context

/// A login prompt (`AuthPrompt`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthPrompt {
    Text {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Secret {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
    Select {
        message: String,
        options: Vec<SelectOption>,
    },
    ManualCode {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
    },
}

impl AuthPrompt {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectOption {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A login notification (`AuthEvent`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthEvent {
    Info {
        message: String,
    },
    AuthUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    DeviceCode {
        user_code: String,
        verification_uri: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interval_seconds: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in_seconds: Option<f64>,
    },
    Progress {
        message: String,
    },
}

impl AuthEvent {
    /// The event as TypeScript passes it to `notify`, integral numbers as integers.
    pub fn to_json(&self) -> Value {
        jsrt::json::normalize_numbers(serde_json::to_value(self).unwrap_or(Value::Null))
    }
}

/// Login callbacks (`AuthInteraction`). `prompt` returns the entered text or selected option id and fails with a
/// message on cancel; `cancel` (when given) aborts once the flow no longer needs the answer (a browser callback
/// arrived first). `signal` aborts the whole login.
pub trait AuthInteraction: Send + Sync {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>>;
    fn notify(&self, event: AuthEvent);
    fn signal(&self) -> Option<AbortSignal> {
        None
    }
}

/// Environment access for auth resolution (`AuthContext`), plus the clock OAuth expiry is judged by (`Date.now()`).
pub trait AuthContext: Send + Sync {
    /// A set, non-blank variable.
    fn env(&self, name: &str) -> Option<String>;
    /// Whether a file exists; a leading `~` is the home directory.
    fn file_exists(&self, path: &str) -> bool;
    fn now_ms(&self) -> f64 {
        now_ms() as f64
    }
}

/// `defaultProviderAuthContext`: the process environment and the real file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultAuthContext;

impl AuthContext for DefaultAuthContext {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.trim().is_empty())
    }

    fn file_exists(&self, path: &str) -> bool {
        let resolved = match path.strip_prefix('~') {
            Some(rest) => match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(format!("{}{rest}", home.to_string_lossy())),
                None => return false,
            },
            None => PathBuf::from(path),
        };
        resolved.exists()
    }
}

/// `overlayEnvAuthContext`: request env values first.
struct OverlayContext<'a> {
    base: &'a dyn AuthContext,
    env: &'a CredentialEnv,
}

impl AuthContext for OverlayContext<'_> {
    fn env(&self, name: &str) -> Option<String> {
        self.env
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned()
            .or_else(|| self.base.env(name))
    }

    fn file_exists(&self, path: &str) -> bool {
        self.base.file_exists(path)
    }

    fn now_ms(&self) -> f64 {
        self.base.now_ms()
    }
}

// ---------------------------------------------------------------------------------------------------- provider auth

/// Api-key auth: a stored key/provider env plus ambient sources (`ApiKeyAuth`).
pub trait ApiKeyAuth: Send + Sync {
    fn name(&self) -> &str;

    /// Interactive setup; `None` for ambient-only providers.
    fn login<'a>(
        &'a self,
        _interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, String>>> {
        None
    }

    /// Side-effect-free availability check; `None` means `Models` resolves auth instead.
    fn check<'a>(
        &'a self,
        _ctx: &'a dyn AuthContext,
        _credential: Option<&'a ApiKeyCredential>,
    ) -> Option<BoxFuture<'a, Result<Option<AuthCheck>, String>>> {
        None
    }

    /// Resolve from the credential and/or ambient sources; `None` is "not configured".
    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>>;
}

/// OAuth auth (`OAuthAuth`): `login` and `refresh` produce a credential, `to_auth` derives request auth from it.
pub trait OAuthAuth: Send + Sync {
    fn name(&self) -> &str;
    fn is_subscription(&self) -> bool {
        false
    }
    fn login_label(&self) -> Option<&str> {
        None
    }
    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>>;
    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>>;
    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String>;
}

/// A provider's auth methods; at least one is present.
#[derive(Clone, Default)]
pub struct ProviderAuth {
    pub api_key: Option<Arc<dyn ApiKeyAuth>>,
    pub oauth: Option<Arc<dyn OAuthAuth>>,
}

fn stored_key_result(credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
    let credential = credential?;
    let key = credential.key.as_ref().filter(|k| !k.is_empty())?;
    Some(AuthResult {
        auth: ModelAuth {
            api_key: Some(key.clone()),
            ..Default::default()
        },
        env: credential.env.clone(),
        source: Some("stored credential".into()),
    })
}

/// `envApiKeyAuth`: a stored key wins, else the first set env var; `login` prompts for the key.
pub struct EnvApiKeyAuth {
    pub name: String,
    pub env_vars: Vec<String>,
}

impl EnvApiKeyAuth {
    pub fn new(name: &str, env_vars: &[&str]) -> Self {
        EnvApiKeyAuth {
            name: name.into(),
            env_vars: env_vars.iter().map(|v| (*v).to_owned()).collect(),
        }
    }

    fn resolve_now(&self, ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        stored_key_result(credential).or_else(|| {
            self.env_vars.iter().find_map(|var| {
                ctx.env(var).map(|value| AuthResult {
                    auth: ModelAuth {
                        api_key: Some(value),
                        ..Default::default()
                    },
                    env: None,
                    source: Some(var.clone()),
                })
            })
        })
    }
}

impl ApiKeyAuth for EnvApiKeyAuth {
    fn name(&self) -> &str {
        &self.name
    }

    fn login<'a>(
        &'a self,
        interaction: &'a dyn AuthInteraction,
    ) -> Option<BoxFuture<'a, Result<ApiKeyCredential, String>>> {
        Some(Box::pin(async move {
            let key = interaction
                .prompt(
                    AuthPrompt::Secret {
                        message: format!("Enter {}", self.name),
                        placeholder: None,
                    },
                    None,
                )
                .await?;
            Ok(ApiKeyCredential {
                key: Some(key),
                env: None,
            })
        }))
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        let result = self.resolve_now(ctx, credential);
        Box::pin(async move { Ok(result) })
    }
}

/// Anthropic's api-key auth: a stored key, else `ANTHROPIC_AUTH_TOKEN` as a bearer header, else
/// `ANTHROPIC_OAUTH_TOKEN` or `ANTHROPIC_API_KEY` as the key.
struct AnthropicApiKeyAuth;

impl AnthropicApiKeyAuth {
    fn resolve_now(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        if let Some(result) = stored_key_result(credential) {
            return Some(result);
        }
        if let Some(token) = ctx.env(crate::env::ANTHROPIC_AUTH_TOKEN_ENV) {
            let mut headers = ProviderHeaders::new();
            headers.insert("Authorization".into(), Some(format!("Bearer {token}")));
            return Some(AuthResult {
                auth: ModelAuth {
                    headers: Some(headers),
                    ..Default::default()
                },
                env: None,
                source: Some(crate::env::ANTHROPIC_AUTH_TOKEN_ENV.into()),
            });
        }
        EnvApiKeyAuth::new(
            "Anthropic API key",
            &[crate::env::ANTHROPIC_OAUTH_TOKEN_ENV, crate::env::ANTHROPIC_API_KEY_ENV],
        )
        .resolve_now(ctx, None)
    }
}

impl ApiKeyAuth for AnthropicApiKeyAuth {
    fn name(&self) -> &str {
        "Anthropic API key"
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        let result = Self::resolve_now(ctx, credential);
        Box::pin(async move { Ok(result) })
    }
}

const VERTEX_ADC_PATH: &str = "~/.config/gcloud/application_default_credentials.json";

/// Vertex: an explicit API key, or Application Default Credentials (a credentials file plus project and location).
struct VertexAuth;

impl VertexAuth {
    fn resolve_now(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        let stored_key = credential.and_then(|c| c.key.clone());
        if let Some(key) = stored_key.clone().or_else(|| ctx.env("GOOGLE_CLOUD_API_KEY"))
            && !key.is_empty()
        {
            return Some(AuthResult {
                auth: ModelAuth {
                    api_key: Some(key),
                    ..Default::default()
                },
                env: None,
                source: Some(
                    if stored_key.is_some() {
                        "stored credential"
                    } else {
                        "GOOGLE_CLOUD_API_KEY"
                    }
                    .into(),
                ),
            });
        }
        let stored_env = |name: &str| credential.and_then(|c| c.env.as_ref()?.get(name).cloned());
        let adc_path =
            stored_env("GOOGLE_APPLICATION_CREDENTIALS").or_else(|| ctx.env("GOOGLE_APPLICATION_CREDENTIALS"));
        let has_credentials = ctx.file_exists(adc_path.as_deref().unwrap_or(VERTEX_ADC_PATH));
        let project = stored_env("GOOGLE_CLOUD_PROJECT")
            .or_else(|| ctx.env("GOOGLE_CLOUD_PROJECT"))
            .or_else(|| ctx.env("GCLOUD_PROJECT"));
        let location = stored_env("GOOGLE_CLOUD_LOCATION").or_else(|| ctx.env("GOOGLE_CLOUD_LOCATION"));
        let set = |v: &Option<String>| v.as_ref().is_some_and(|v| !v.is_empty());
        if has_credentials && set(&project) && set(&location) {
            return Some(AuthResult {
                auth: ModelAuth::default(),
                env: credential.and_then(|c| c.env.clone()),
                source: Some(
                    if credential.is_some() {
                        "stored credential"
                    } else {
                        "gcloud application default credentials"
                    }
                    .into(),
                ),
            });
        }
        None
    }
}

impl ApiKeyAuth for VertexAuth {
    fn name(&self) -> &str {
        "Google Cloud credentials"
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        let result = Self::resolve_now(ctx, credential);
        Box::pin(async move { Ok(result) })
    }
}

/// A built-in subscription OAuth flow (Anthropic, OpenAI Codex, GitHub Copilot) bound to a transport; `clock` is
/// `Date.now()` for the expiry of refreshed tokens.
pub struct BuiltinOAuth {
    pub provider: OAuthProvider,
    pub transport: Arc<dyn Transport>,
    pub clock: Clock,
    /// The browser login's callback server listens here instead of the registered port (`Some(0)`: any free port;
    /// the redirect URI keeps the registered one).
    pub callback_port: Option<u16>,
}

impl OAuthAuth for BuiltinOAuth {
    fn name(&self) -> &str {
        self.provider.name()
    }

    fn is_subscription(&self) -> bool {
        true
    }

    fn login<'a>(&'a self, interaction: &'a dyn AuthInteraction) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            let env = SystemLoginEnv;
            let mut ctx = LoginContext::new(self.transport.as_ref(), interaction, &env);
            if let Some(signal) = interaction.signal() {
                ctx.signal = signal;
            }
            ctx.callback_port = self.callback_port;
            self.provider.login(&ctx).await
        })
    }

    fn refresh<'a>(
        &'a self,
        credential: &'a OAuthCredential,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'a, Result<OAuthCredential, String>> {
        Box::pin(async move {
            self.provider
                .refresh(
                    self.transport.as_ref(),
                    credential,
                    (self.clock)() as f64,
                    signal.as_ref(),
                )
                .await
        })
    }

    fn to_auth(&self, credential: &OAuthCredential) -> Result<ModelAuth, String> {
        Ok(self.provider.to_auth(credential))
    }
}

/// The auth handlers of a built-in provider, OAuth flows going through `transport`, with an injected clock for OAuth
/// token expiry.
pub fn builtin_provider_auth_with_clock(
    provider_id: &str,
    transport: Arc<dyn Transport>,
    clock: Clock,
) -> Option<ProviderAuth> {
    let oauth = |provider| -> Option<Arc<dyn OAuthAuth>> {
        Some(Arc::new(BuiltinOAuth {
            provider,
            transport: Arc::clone(&transport),
            clock,
            callback_port: None,
        }))
    };
    let env =
        |name: &str, vars: &[&str]| -> Option<Arc<dyn ApiKeyAuth>> { Some(Arc::new(EnvApiKeyAuth::new(name, vars))) };
    Some(match provider_id {
        "anthropic" => ProviderAuth {
            api_key: builtin_api_key_auth(provider_id),
            oauth: oauth(OAuthProvider::Anthropic),
        },
        // OpenRouter's OAuth login (crate::oauth_openrouter) exchanges the code for a permanent API key.
        "openrouter" => ProviderAuth {
            api_key: env("OpenRouter API key", &["OPENROUTER_API_KEY"]),
            oauth: Some(Arc::new(crate::oauth_openrouter::OpenRouterOAuth {
                transport: Arc::clone(&transport),
            })),
        },
        "openai" => ProviderAuth {
            api_key: env("OpenAI API key", &["OPENAI_API_KEY"]),
            oauth: None,
        },
        "openai-codex" => ProviderAuth {
            api_key: None,
            oauth: oauth(OAuthProvider::OpenAiCodex),
        },
        "github-copilot" => ProviderAuth {
            api_key: env("GitHub Copilot token", &["COPILOT_GITHUB_TOKEN"]),
            oauth: oauth(OAuthProvider::GitHubCopilot),
        },
        "google" => ProviderAuth {
            api_key: env("Gemini API key", &["GEMINI_API_KEY"]),
            oauth: None,
        },
        "google-vertex" | "cloudflare-workers-ai" | "cloudflare-ai-gateway" => ProviderAuth {
            api_key: builtin_api_key_auth(provider_id),
            oauth: None,
        },
        _ => return None,
    })
}

/// amazon-bedrock.ts `bedrockAuth.resolve`: a stored key (with the credential's env), else
/// `AWS_BEARER_TOKEN_BEDROCK`, else an AWS profile (the stored credential's `AWS_PROFILE` and env, or the
/// environment's), else access keys, an ECS task role or a web identity token; no key in those cases, so the SDK's
/// credential chain signs the request.
fn bedrock_auth(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
    let var = |name: &str| ctx.env(name);
    let result = |api_key: Option<String>, env: Option<CredentialEnv>, source: &str| AuthResult {
        auth: ModelAuth {
            api_key,
            ..Default::default()
        },
        env,
        source: Some(source.into()),
    };
    let stored_profile = credential.and_then(|c| c.env.as_ref()?.get("AWS_PROFILE").filter(|p| !p.is_empty()).cloned());
    if let Some(key) = credential.and_then(|c| c.key.clone()).filter(|k| !k.is_empty()) {
        return Some(result(
            Some(key),
            credential.and_then(|c| c.env.clone()),
            "stored credential",
        ));
    }
    if var("AWS_BEARER_TOKEN_BEDROCK").is_some() {
        return Some(result(None, None, "AWS_BEARER_TOKEN_BEDROCK"));
    }
    if stored_profile.is_some() || var("AWS_PROFILE").is_some() {
        let source = if stored_profile.is_some() {
            "stored credential"
        } else {
            "AWS_PROFILE"
        };
        return Some(result(None, credential.and_then(|c| c.env.clone()), source));
    }
    if var("AWS_ACCESS_KEY_ID").is_some() && var("AWS_SECRET_ACCESS_KEY").is_some() {
        return Some(result(None, None, "AWS access keys"));
    }
    if var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some() || var("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some() {
        return Some(result(None, None, "ECS task role"));
    }
    var("AWS_WEB_IDENTITY_TOKEN_FILE").map(|_| result(None, None, "web identity token"))
}

/// Amazon Bedrock's api-key auth ([`bedrock_auth`]).
struct BedrockAuth;

impl ApiKeyAuth for BedrockAuth {
    fn name(&self) -> &str {
        "Amazon Bedrock credentials"
    }

    fn resolve<'a>(
        &'a self,
        ctx: &'a dyn AuthContext,
        credential: Option<&'a ApiKeyCredential>,
    ) -> BoxFuture<'a, Result<Option<AuthResult>, String>> {
        let result = bedrock_auth(ctx, credential);
        Box::pin(async move { Ok(result) })
    }
}

/// The built-in api-key auth of the providers whose resolution is more than "the stored key, else an environment
/// key": Anthropic (`ANTHROPIC_AUTH_TOKEN` as a bearer header), Google Vertex (Application Default Credentials and
/// the project/location env), Cloudflare (account and gateway ids merged per field from the stored credential and
/// the environment; the AI Gateway authenticates by `cf-aig-authorization`) and Amazon Bedrock (a key, else the AWS
/// credential chain signs). `None` for every other provider.
pub fn builtin_api_key_auth(provider_id: &str) -> Option<Arc<dyn ApiKeyAuth>> {
    use crate::cloudflare::{CloudflareAuth, CloudflareAuthKind};
    let auth: Arc<dyn ApiKeyAuth> = match provider_id {
        "anthropic" => Arc::new(AnthropicApiKeyAuth),
        "google-vertex" => Arc::new(VertexAuth),
        "cloudflare-workers-ai" => Arc::new(CloudflareAuth(CloudflareAuthKind::WorkersAi)),
        "cloudflare-ai-gateway" => Arc::new(CloudflareAuth(CloudflareAuthKind::AiGateway)),
        "amazon-bedrock" => Arc::new(BedrockAuth),
        _ => return None,
    };
    Some(auth)
}

/// `provider.filterModels(models, credential)`, which only GitHub Copilot has: with an OAuth credential listing
/// `availableModelIds` (all strings), only those models; otherwise, and for every other provider, all. `credential`
/// is read only for a provider that filters.
pub fn filter_models<'m, M>(
    provider: &str,
    models: &'m [M],
    id: impl Fn(&M) -> &str,
    credential: impl FnOnce() -> Option<Credential>,
) -> Vec<&'m M> {
    if provider != "github-copilot" {
        return models.iter().collect();
    }
    let credential = credential();
    let available = match credential.as_ref() {
        Some(Credential::OAuth(c)) => c
            .extra
            .get("availableModelIds")
            .and_then(Value::as_array)
            .and_then(|ids| ids.iter().map(|v| v.as_str()).collect::<Option<Vec<&str>>>()),
        _ => None,
    };
    match available {
        Some(ids) => models.iter().filter(|m| ids.contains(&id(m))).collect(),
        None => models.iter().collect(),
    }
}

// ------------------------------------------------------------------------------------------------ credential stores

/// The write callback of [`CredentialStore::modify`]: sees the current credential, returns the new one or `None` to
/// leave it unchanged.
pub type ModifyFn<'a> =
    Box<dyn FnOnce(Option<Credential>) -> BoxFuture<'a, Result<Option<Credential>, AuthError>> + Send + 'a>;

/// A failed `modify`: the callback's own error, the signal, or the store itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    Callback(AuthError),
    Aborted,
    Store(String),
}

/// App-owned credential storage keyed by provider id (`CredentialStore`). `modify` is the only write path and is
/// serialized per provider; it resolves with the post-write credential. A write whose signal aborts while queued
/// never runs.
pub trait CredentialStore: Send + Sync {
    fn read<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>>;
    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, String>>;
    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: ModifyFn<'a>,
        signal: Option<&'a AbortSignal>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>>;
    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

/// Runs `work` unless `signal` aborts first; biased, so an abort seen at any poll wins and a queued write whose
/// signal fired never starts.
async fn unless_aborted<T>(
    signal: Option<&AbortSignal>,
    work: impl Future<Output = Result<T, StoreError>>,
) -> Result<T, StoreError> {
    if is_aborted(signal) {
        return Err(StoreError::Aborted);
    }
    tokio::select! {
        biased;
        () = crate::http::cancelled(signal) => Err(StoreError::Aborted),
        result = work => result,
    }
}

/// `InMemoryCredentialStore`: reads see the current map at once; writes per provider run one at a time in arrival
/// order; `list` is in insertion order.
#[derive(Default)]
pub struct InMemoryCredentialStore {
    credentials: Mutex<IndexMap<String, Credential>>,
    locks: Mutex<IndexMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl InMemoryCredentialStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn data(&self) -> std::sync::MutexGuard<'_, IndexMap<String, Credential>> {
        self.credentials.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_for(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(provider_id.to_owned())
                .or_default(),
        )
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>> {
        let value = self.data().get(provider_id).cloned();
        Box::pin(async move { Ok(value) })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, String>> {
        let out = self
            .data()
            .iter()
            .map(|(id, c)| CredentialInfo {
                provider_id: id.clone(),
                kind: c.kind(),
            })
            .collect();
        Box::pin(async move { Ok(out) })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: ModifyFn<'a>,
        signal: Option<&'a AbortSignal>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(unless_aborted(signal, async move {
            let lock = self.lock_for(provider_id);
            let _guard = lock.lock().await;
            let current = self.data().get(provider_id).cloned();
            let next = f(current.clone()).await.map_err(StoreError::Callback)?;
            if is_aborted(signal) {
                return Err(StoreError::Aborted);
            }
            match next {
                Some(next) => {
                    self.data().insert(provider_id.to_owned(), next.clone());
                    Ok(Some(next))
                }
                None => Ok(current),
            }
        }))
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let lock = self.lock_for(provider_id);
            let _guard = lock.lock().await;
            self.data().shift_remove(provider_id);
            Ok(())
        })
    }
}

/// `auth.json` as a credential store, sharing the TypeScript product's file format and `proper-lockfile` lock, so
/// both can use one agent directory. `read` resolves `$VAR` templates in api keys (`AuthStorage.read`); `modify`
/// keeps the lock across the callback (an OAuth refresh), so concurrent processes refresh once.
pub struct FileCredentialStore {
    path: PathBuf,
    /// Serializes writers of this process; the file lock serializes processes.
    local: tokio::sync::Mutex<()>,
}

impl FileCredentialStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileCredentialStore {
            path: path.into(),
            local: tokio::sync::Mutex::new(()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn storage(&self) -> config::auth::AuthStorage {
        config::auth::AuthStorage::create(self.path.clone())
    }
}

fn parse_credential(value: Value, provider_id: &str) -> Result<Credential, String> {
    serde_json::from_value(value).map_err(|_| format!("Invalid auth.json credential for provider \"{provider_id}\""))
}

impl CredentialStore for FileCredentialStore {
    fn read<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<Option<Credential>, String>> {
        Box::pin(async move {
            let path = self.path.clone();
            let id = provider_id.to_string();
            let value = tokio::task::spawn_blocking(move || config::auth::AuthStorage::create(path).read(&id))
                .await
                .map_err(|e| e.to_string())??;
            value.map(|v| parse_credential(v, provider_id)).transpose()
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<CredentialInfo>, String>> {
        Box::pin(async move {
            let mut storage = self.storage();
            let entries = storage.list()?;
            entries
                .into_iter()
                .map(|(provider_id, kind)| {
                    let kind = serde_json::from_value(kind)
                        .map_err(|_| format!("Invalid auth.json credential for provider \"{provider_id}\""))?;
                    Ok(CredentialInfo { provider_id, kind })
                })
                .collect()
        })
    }

    fn modify<'a>(
        &'a self,
        provider_id: &'a str,
        f: ModifyFn<'a>,
        signal: Option<&'a AbortSignal>,
    ) -> BoxFuture<'a, Result<Option<Credential>, StoreError>> {
        Box::pin(unless_aborted(signal, async move {
            let store = |e: String| StoreError::Store(e);
            let _local = self.local.lock().await;
            let path = self.path.clone();
            let file = tokio::task::spawn_blocking(move || config::auth::LockedAuthFile::open(&path))
                .await
                .map_err(|e| store(e.to_string()))?
                .map_err(store)?;
            let text = file.read().map_err(store)?;
            let mut data = if text.trim().is_empty() {
                Map::new()
            } else {
                config::auth::validate_auth_file(&text).map_err(store)?
            };
            let current = data
                .get(provider_id)
                .cloned()
                .map(|v| parse_credential(v, provider_id))
                .transpose()
                .map_err(store)?;
            let next = f(current.clone()).await.map_err(StoreError::Callback)?;
            if is_aborted(signal) {
                return Err(StoreError::Aborted);
            }
            let result = match next {
                Some(next) => {
                    data.insert(
                        provider_id.to_string(),
                        serde_json::to_value(&next).map_err(|e| store(e.to_string()))?,
                    );
                    file.write(&jsrt::json::stringify_pretty(&Value::Object(data)))
                        .map_err(store)?;
                    Some(next)
                }
                None => current,
            };
            drop(file);
            Ok(result)
        }))
    }

    fn delete<'a>(&'a self, provider_id: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let _local = self.local.lock().await;
            self.storage().delete(provider_id)
        })
    }
}

// ---------------------------------------------------------------------------------------------- auth resolution

const DEFAULT_OAUTH_MINIMUM_VALIDITY_MS: f64 = 5.0 * 60.0 * 1000.0;
const DEFAULT_OAUTH_REFRESH_TIMEOUT: Duration = Duration::from_secs(15);

/// `AuthResolutionOverrides`.
#[derive(Clone, Debug, Default)]
pub struct AuthOverrides {
    pub api_key: Option<String>,
    pub env: Option<CredentialEnv>,
    /// Require this much remaining OAuth validity; five minutes by default.
    pub min_oauth_validity_ms: Option<f64>,
    pub signal: Option<AbortSignal>,
}

/// `resolveProviderAuth`: an explicit key wins; otherwise a stored credential owns the provider (refreshing OAuth
/// that expires within the validity window, once, under the store's lock); ambient sources only when nothing is
/// stored. `Ok(None)` = not configured. Expiry is judged by `ctx.now_ms()`.
pub async fn resolve_provider_auth(
    provider_id: &str,
    auth: &ProviderAuth,
    credentials: &dyn CredentialStore,
    ctx: &dyn AuthContext,
    overrides: &AuthOverrides,
) -> Result<Option<AuthResult>, AuthError> {
    let signal = overrides.signal.clone().unwrap_or_default();
    if signal.aborted() {
        return Err(AuthError::Aborted);
    }
    let overlay;
    let request_ctx: &dyn AuthContext = match &overrides.env {
        Some(env) => {
            overlay = OverlayContext { base: ctx, env };
            &overlay
        }
        None => ctx,
    };
    if let (Some(key), Some(api_key)) = (&overrides.api_key, &auth.api_key) {
        let credential = ApiKeyCredential {
            key: Some(key.clone()),
            env: overrides.env.clone(),
        };
        return resolve_api_key(request_ctx, api_key.as_ref(), provider_id, Some(&credential)).await;
    }
    let stored = credentials.read(provider_id).await.map_err(|e| {
        AuthError::models(
            ModelsErrorCode::Auth,
            format!("Credential store read failed for {provider_id}"),
            Some(&e),
        )
    })?;
    if signal.aborted() {
        return Err(AuthError::Aborted);
    }
    match (stored, &auth.oauth, &auth.api_key) {
        (Some(Credential::OAuth(stored)), Some(oauth), _) => {
            resolve_stored_oauth(
                provider_id,
                oauth.as_ref(),
                stored,
                credentials,
                ctx,
                overrides,
                &signal,
            )
            .await
        }
        (Some(Credential::ApiKey(mut stored)), _, Some(api_key)) => {
            if let Some(env) = &overrides.env {
                stored.env.get_or_insert_default().extend(env.clone());
            }
            resolve_api_key(request_ctx, api_key.as_ref(), provider_id, Some(&stored)).await
        }
        (Some(_), ..) => Ok(None),
        (None, _, Some(api_key)) => resolve_api_key(request_ctx, api_key.as_ref(), provider_id, None).await,
        (None, _, None) => Ok(None),
    }
}

async fn resolve_api_key(
    ctx: &dyn AuthContext,
    api_key: &dyn ApiKeyAuth,
    provider_id: &str,
    credential: Option<&ApiKeyCredential>,
) -> Result<Option<AuthResult>, AuthError> {
    api_key.resolve(ctx, credential).await.map_err(|e| {
        AuthError::models(
            ModelsErrorCode::Auth,
            format!("API key auth failed for provider {provider_id}"),
            Some(&e),
        )
    })
}

/// Double-checked OAuth refresh: an expiring token takes the store lock, is re-checked under it, refreshed once, and
/// the rotated credential is persisted before the lock is released.
async fn resolve_stored_oauth(
    provider_id: &str,
    oauth: &dyn OAuthAuth,
    stored: OAuthCredential,
    credentials: &dyn CredentialStore,
    ctx: &dyn AuthContext,
    overrides: &AuthOverrides,
    signal: &AbortSignal,
) -> Result<Option<AuthResult>, AuthError> {
    let minimum = DEFAULT_OAUTH_MINIMUM_VALIDITY_MS.max(overrides.min_oauth_validity_ms.unwrap_or(0.0));
    let expires_soon = move |c: &OAuthCredential| ctx.now_ms() + minimum >= c.expires;
    let mut credential = stored;
    if expires_soon(&credential) {
        let refresh_signal = signal.clone();
        let refresh: ModifyFn<'_> = Box::new(move |current| {
            Box::pin(async move {
                let Some(Credential::OAuth(current)) = current else {
                    return Ok(None); // logged out meanwhile
                };
                if !expires_soon(&current) {
                    return Ok(None); // refreshed by someone else
                }
                let result = tokio::time::timeout(
                    DEFAULT_OAUTH_REFRESH_TIMEOUT,
                    oauth.refresh(&current, Some(refresh_signal)),
                )
                .await
                .unwrap_or_else(|_| Err(crate::provider::OPERATION_TIMED_OUT.into()));
                result.map(|c| Some(Credential::OAuth(c))).map_err(|e| {
                    AuthError::models(
                        ModelsErrorCode::OAuth,
                        format!("OAuth refresh failed for {provider_id}"),
                        Some(&e),
                    )
                })
            })
        });
        let post = match credentials.modify(provider_id, refresh, Some(signal)).await {
            Ok(post) => post,
            Err(StoreError::Callback(error)) => return Err(error),
            Err(StoreError::Aborted) => return Err(AuthError::Aborted),
            Err(StoreError::Store(e)) => {
                return Err(AuthError::models(
                    ModelsErrorCode::Auth,
                    format!("Credential store modify failed for {provider_id}"),
                    Some(&e),
                ));
            }
        };
        let Some(Credential::OAuth(post)) = post else {
            return Ok(None);
        };
        credential = post;
        if overrides.min_oauth_validity_ms.is_some() && expires_soon(&credential) {
            return Err(AuthError::models(
                ModelsErrorCode::OAuth,
                format!("OAuth refresh returned a token that expires too soon for {provider_id}"),
                None,
            ));
        }
    }
    let auth = oauth.to_auth(&credential).map_err(|e| {
        AuthError::models(
            ModelsErrorCode::OAuth,
            format!("OAuth auth derivation failed for {provider_id}"),
            Some(&e),
        )
    })?;
    Ok(Some(AuthResult {
        auth,
        env: None,
        source: Some("OAuth".into()),
    }))
}

/// A provider's auth bound to a credential store and context: `resolveProviderAuth` as `ImagesModels.getAuth` uses
/// it, so image providers get stored credentials and OAuth refresh like chat providers.
pub struct StoredProviderAuth {
    pub auth: ProviderAuth,
    pub credentials: Arc<dyn CredentialStore>,
    pub ctx: Arc<dyn AuthContext>,
}

impl crate::images::ImagesAuth for StoredProviderAuth {
    fn resolve<'a>(
        &'a self,
        provider_id: &'a str,
        overrides: &'a crate::images::ImagesAuthOverrides,
    ) -> BoxFuture<'a, Result<Option<crate::images::ImagesAuthResult>, String>> {
        Box::pin(async move {
            let overrides = AuthOverrides {
                api_key: overrides.api_key.clone(),
                env: overrides.env.as_ref().map(sorted_env),
                ..Default::default()
            };
            let result = resolve_provider_auth(
                provider_id,
                &self.auth,
                self.credentials.as_ref(),
                self.ctx.as_ref(),
                &overrides,
            )
            .await
            .map_err(|e| e.message())?;
            Ok(result.map(|r| crate::images::ImagesAuthResult {
                api_key: r.auth.api_key,
                headers: r.auth.headers,
                base_url: r.auth.base_url,
                env: r.env.map(|env| env.into_iter().collect()),
                source: r.source,
            }))
        })
    }
}

// ------------------------------------------------------------------------------------------------------ Headers

/// `mergeHeaders`: names compare case-insensitively; the override's spelling wins.
pub fn merge_headers(base: Option<&ProviderHeaders>, override_: Option<&ProviderHeaders>) -> Option<ProviderHeaders> {
    if base.is_none() && override_.is_none() {
        return None;
    }
    let mut merged = base.cloned().unwrap_or_default();
    for (name, value) in override_.into_iter().flatten() {
        merged.retain(|existing, _| !existing.eq_ignore_ascii_case(name));
        merged.insert(name.clone(), value.clone());
    }
    Some(merged)
}

/// Provider env in a stable order.
fn sorted_env(env: &crate::env::ProviderEnv) -> CredentialEnv {
    let mut entries: Vec<(&String, &String)> = env.iter().collect();
    entries.sort();
    entries.into_iter().map(|(k, v)| (k.clone(), v.clone())).collect()
}
