//! Settings, credentials, project trust and agent paths for the Rust product.
//!
//! Every file this crate reads or writes (`settings.json`, `auth.json`, `models-store.json`, `trust.json`) keeps the byte format the
//! TypeScript product writes (`JSON.stringify(value, null, 2)`) and takes the same `proper-lockfile` directory
//! locks, so both implementations can share one agent directory.

pub mod auth;
pub mod browser;
pub mod env;
pub mod git;
pub mod json;
pub mod keys;
pub mod lock;
pub mod models_store;
pub mod paths;
pub mod profile_backup;
pub mod providers;
pub mod radius;
pub mod settings;
pub mod trust;

/// The product version (`VERSION` of config.ts): packages/coding-agent/package.json's `version`.
pub fn version() -> &'static str {
    static VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let package: serde_json::Value =
            serde_json::from_str(include_str!("../../../packages/coding-agent/package.json"))
                .expect("packages/coding-agent/package.json parses");
        package["version"]
            .as_str()
            .expect("package.json has a version")
            .to_string()
    });
    &VERSION
}
