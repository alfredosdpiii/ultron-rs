//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `crates/workspace-checks` keeps both lists whole.

// Support modules are declared here, once; the files reach them through `crate::`.

#[path = "claude_subscription_support/isolate.rs"]
mod isolate;

#[path = "auth.rs"]
mod auth;
#[path = "auth_rest.rs"]
mod auth_rest;
#[path = "bedrock_credentials.rs"]
mod bedrock_credentials;
#[path = "bedrock_http1.rs"]
mod bedrock_http1;
#[path = "bedrock_proxy.rs"]
mod bedrock_proxy;
#[path = "behavior_rest.rs"]
mod behavior_rest;
#[path = "claude_code_provider.rs"]
mod claude_code_provider;
#[path = "claude_subscription_rewrite.rs"]
mod claude_subscription_rewrite;
#[path = "codex_websocket.rs"]
mod codex_websocket;
#[path = "codex_websocket_cache.rs"]
mod codex_websocket_cache;
#[path = "codex_zstd.rs"]
mod codex_zstd;
#[path = "idle_timeout.rs"]
mod idle_timeout;
#[path = "images.rs"]
mod images;
#[path = "images_timeout.rs"]
mod images_timeout;
#[path = "mistral_timeout.rs"]
mod mistral_timeout;
#[path = "model_config.rs"]
mod model_config;
#[path = "model_registry.rs"]
mod model_registry;
#[path = "model_resolver.rs"]
mod model_resolver;
#[path = "model_scope_subscription.rs"]
mod model_scope_subscription;
#[path = "oauth.rs"]
mod oauth;
#[path = "oauth_login.rs"]
mod oauth_login;
#[path = "oauth_openrouter.rs"]
mod oauth_openrouter;
#[path = "pi_messages_passthrough.rs"]
mod pi_messages_passthrough;
#[path = "providers.rs"]
mod providers;
#[path = "providers_more.rs"]
mod providers_more;
#[path = "providers_rest.rs"]
mod providers_rest;
#[path = "radius_callback_port.rs"]
mod radius_callback_port;
#[path = "retry.rs"]
mod retry;
#[path = "retry_after_date.rs"]
mod retry_after_date;
#[path = "stream_errors.rs"]
mod stream_errors;
#[path = "units.rs"]
mod units;
#[path = "vertex_adc.rs"]
mod vertex_adc;
#[path = "vertex_mtls.rs"]
mod vertex_mtls;
