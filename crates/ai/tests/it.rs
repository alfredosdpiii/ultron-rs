//! This crate's integration tests as one test binary: each `tests/*.rs` file is a module of it, so the tests link
//! once rather than once per file. A file that re-runs or exits its own test binary (`current_exe`,
//! `process::exit`) stays a test binary of its own in Cargo.toml. `every_test_file_is_built` keeps both lists whole.

// Support modules (`mod common;`) stay declared by each file that uses them, so each file also builds alone.
#![allow(clippy::duplicate_mod)]

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
#[path = "legacy.rs"]
mod legacy;
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

#[test]
fn every_test_file_is_built() {
    let (merged, manifest) = (include_str!("it.rs"), include_str!("../Cargo.toml"));
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    for entry in std::fs::read_dir(tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && name != "it.rs" {
            let listed = merged.contains(&format!("#[path = \"{name}\"]"))
                || manifest.contains(&format!("path = \"tests/{name}\""));
            assert!(
                listed,
                "tests/{name} is neither a module of tests/it.rs nor a [[test]] of Cargo.toml"
            );
        }
    }
}
