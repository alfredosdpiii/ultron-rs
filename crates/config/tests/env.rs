//! `config::env`: the offline flag and the model settings environment variables override.

use config::env::*;

fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

#[test]
fn config_env_offline_is_a_truthy_flag() {
    for (value, expected) in [
        ("1", true),
        ("true", true),
        ("TRUE", true),
        ("Yes", true),
        ("0", false),
        ("", false),
        ("no", false),
        ("false", false),
        (" 1", false),
        ("on", false),
    ] {
        assert_eq!(offline(&env(&[(OFFLINE_ENV, value)])), expected, "{value:?}");
    }
    assert!(!offline(&env(&[])));
}

#[test]
fn config_env_model_overrides_need_well_formed_refs_and_non_blank_aliases() {
    let var = env(&[
        (RLM_FRAME_MODEL_ENV, " groq/llama "),
        (REVIEW_MODEL_ENV, "a//b"),
        (CLAUDE_FRAME_MODEL_ENV, "sonnet"),
        (CLAUDE_CHILD_MODEL_ENV, " haiku "),
        (CLAUDE_MODEL_ENV, "  "),
    ]);
    assert_eq!(
        model_env_overrides(&var),
        [
            ("rlm.frameModel", RLM_FRAME_MODEL_ENV, "groq/llama".to_string()),
            ("claudeCode.childModel", CLAUDE_CHILD_MODEL_ENV, "haiku".to_string()),
        ]
    );
    assert_eq!(env_model(&env(&[(REVIEW_MODEL_ENV, "a/b/")]), REVIEW_MODEL_ENV), None);
    assert_eq!(env_model(&env(&[(REVIEW_MODEL_ENV, "a/b c")]), REVIEW_MODEL_ENV), None);
}
