//! Built-in provider auth handlers (`EnvApiKeyAuth`, Anthropic, Vertex, Bedrock, and subscription OAuth).

use super::{
    ApiKeyAuth, ApiKeyCredential, AuthContext, AuthInteraction, AuthPrompt, AuthResult, CredentialEnv, ModelAuth,
    OAuthAuth, OAuthCredential, ProviderAuth,
};
use crate::http::{AbortSignal, BoxFuture, Transport};
use crate::oauth::OAuthProvider;
use crate::oauth_flow::{Clock, SystemLoginEnv};
use crate::oauth_login::LoginContext;
use crate::options::ProviderHeaders;
use std::sync::Arc;

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
    fn resolve_api_key(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        let stored_key = credential.and_then(|c| c.key.clone());
        let key = stored_key.clone().or_else(|| ctx.env("GOOGLE_CLOUD_API_KEY"))?;
        if key.is_empty() {
            return None;
        }
        let source = if stored_key.is_some() {
            "stored credential"
        } else {
            "GOOGLE_CLOUD_API_KEY"
        };
        Some(AuthResult {
            auth: ModelAuth {
                api_key: Some(key),
                ..Default::default()
            },
            env: None,
            source: Some(source.into()),
        })
    }

    fn resolve_adc(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        let stored_env = |name: &str| credential.and_then(|c| c.env.as_ref()?.get(name).cloned());
        let adc_path =
            stored_env("GOOGLE_APPLICATION_CREDENTIALS").or_else(|| ctx.env("GOOGLE_APPLICATION_CREDENTIALS"));
        let has_credentials = ctx.file_exists(adc_path.as_deref().unwrap_or(VERTEX_ADC_PATH));
        let project = stored_env("GOOGLE_CLOUD_PROJECT")
            .or_else(|| ctx.env("GOOGLE_CLOUD_PROJECT"))
            .or_else(|| ctx.env("GCLOUD_PROJECT"));
        let location = stored_env("GOOGLE_CLOUD_LOCATION").or_else(|| ctx.env("GOOGLE_CLOUD_LOCATION"));
        let set = |v: &Option<String>| v.as_ref().is_some_and(|v| !v.is_empty());
        if !(has_credentials && set(&project) && set(&location)) {
            return None;
        }
        let source = if credential.is_some() {
            "stored credential"
        } else {
            "gcloud application default credentials"
        };
        Some(AuthResult {
            auth: ModelAuth::default(),
            env: credential.and_then(|c| c.env.clone()),
            source: Some(source.into()),
        })
    }

    fn resolve_now(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
        Self::resolve_api_key(ctx, credential).or_else(|| Self::resolve_adc(ctx, credential))
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

fn bedrock_result(api_key: Option<String>, env: Option<CredentialEnv>, source: &str) -> AuthResult {
    AuthResult {
        auth: ModelAuth {
            api_key,
            ..Default::default()
        },
        env,
        source: Some(source.into()),
    }
}

fn bedrock_ambient_source(ctx: &dyn AuthContext) -> Option<&'static str> {
    if ctx.env("AWS_ACCESS_KEY_ID").is_some() && ctx.env("AWS_SECRET_ACCESS_KEY").is_some() {
        return Some("AWS access keys");
    }
    if ctx.env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || ctx.env("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return Some("ECS task role");
    }
    ctx.env("AWS_WEB_IDENTITY_TOKEN_FILE").map(|_| "web identity token")
}

/// amazon-bedrock.ts `bedrockAuth.resolve`: a stored key (with the credential's env), else
/// `AWS_BEARER_TOKEN_BEDROCK`, else an AWS profile (the stored credential's `AWS_PROFILE` and env, or the
/// environment's), else access keys, an ECS task role or a web identity token; no key in those cases, so the SDK's
/// credential chain signs the request.
fn bedrock_auth(ctx: &dyn AuthContext, credential: Option<&ApiKeyCredential>) -> Option<AuthResult> {
    if let Some(key) = credential.and_then(|c| c.key.clone()).filter(|k| !k.is_empty()) {
        return Some(bedrock_result(
            Some(key),
            credential.and_then(|c| c.env.clone()),
            "stored credential",
        ));
    }
    if ctx.env("AWS_BEARER_TOKEN_BEDROCK").is_some() {
        return Some(bedrock_result(None, None, "AWS_BEARER_TOKEN_BEDROCK"));
    }
    let stored_profile = credential.and_then(|c| c.env.as_ref()?.get("AWS_PROFILE").filter(|p| !p.is_empty()).cloned());
    if stored_profile.is_some() || ctx.env("AWS_PROFILE").is_some() {
        let source = if stored_profile.is_some() {
            "stored credential"
        } else {
            "AWS_PROFILE"
        };
        return Some(bedrock_result(None, credential.and_then(|c| c.env.clone()), source));
    }
    bedrock_ambient_source(ctx).map(|source| bedrock_result(None, None, source))
}

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
