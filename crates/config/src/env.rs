//! The environment variables config-level code owns: offline mode, the version-check switch, the model settings a
//! variable overrides (worker-settings.ts `MODEL_SETTING_ENV`) and the internal debug switch.

pub const OFFLINE_ENV: &str = "PI_OFFLINE";
pub const SKIP_VERSION_CHECK_ENV: &str = "PI_SKIP_VERSION_CHECK";
pub const DEBUG_INTERNAL_ENV: &str = "ULTRON_DEBUG_INTERNAL";
/// model-settings.ts `FRAME_MODEL_ENV`.
pub const RLM_FRAME_MODEL_ENV: &str = "ULTRON_RLM_FRAME_MODEL";
/// model-settings.ts `REVIEW_MODEL_ENV`.
pub const REVIEW_MODEL_ENV: &str = "ULTRON_REVIEW_MODEL";
pub const CLAUDE_FRAME_MODEL_ENV: &str = "ULTRON_CLAUDE_FRAME_MODEL";
pub const CLAUDE_CHILD_MODEL_ENV: &str = "ULTRON_CLAUDE_CHILD_MODEL";
pub const CLAUDE_MODEL_ENV: &str = "ULTRON_CLAUDE_MODEL";

/// `MODEL_SETTING_ENV`: model settings and the environment variables that override them in a worker.
pub const MODEL_SETTING_ENV: [(&str, &str); 5] = [
    ("rlm.frameModel", RLM_FRAME_MODEL_ENV),
    ("review.model", REVIEW_MODEL_ENV),
    ("claudeCode.frameModel", CLAUDE_FRAME_MODEL_ENV),
    ("claudeCode.childModel", CLAUDE_CHILD_MODEL_ENV),
    ("claudeCode.model", CLAUDE_MODEL_ENV),
];

/// main.ts `isTruthyEnvFlag(process.env.PI_OFFLINE)` (`--offline` sets it to `1`): `1`, `true` or `yes`, the words
/// in any case.
pub fn offline(var: &dyn Fn(&str) -> Option<String>) -> bool {
    var(OFFLINE_ENV).is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"))
}

/// `envModel`: the trimmed value of `name` when it is a `provider/model` reference.
pub fn env_model(var: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    let value = var(name)?;
    let value = jsrt::string::trim(&value);
    model_scope::is_model_ref(value).then(|| value.to_string())
}

/// `modelEnvOverrides`: `(setting, variable, value)` for each [`MODEL_SETTING_ENV`] variable that overrides its
/// setting. A `provider/model` variable counts only when well formed (as the runtime reads it); a Claude Code alias
/// (`claudeCode.childModel`, `claudeCode.model`) when not blank.
pub fn model_env_overrides(var: &dyn Fn(&str) -> Option<String>) -> Vec<(&'static str, &'static str, String)> {
    MODEL_SETTING_ENV
        .into_iter()
        .filter_map(|(key, name)| {
            let value = if key.starts_with("claudeCode.") && key != "claudeCode.frameModel" {
                var(name)
                    .map(|v| jsrt::string::trim(&v).to_string())
                    .filter(|v| !v.is_empty())
            } else {
                env_model(var, name)
            };
            Some((key, name, value?))
        })
        .collect()
}
