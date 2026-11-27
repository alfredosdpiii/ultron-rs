use crate::provider::compat_flag;
use crate::types::Model;
use serde_json::{Map, Value};

/// Resolved compat (`getCompat`): detected from provider and URL, overridden by `model.compat`.
pub(super) struct Compat {
    pub(super) supports_store: bool,
    pub(super) supports_developer_role: bool,
    pub(super) supports_reasoning_effort: bool,
    pub(super) supports_usage_in_streaming: bool,
    pub(super) supports_finish_reason: bool,
    pub(super) max_tokens_field: String,
    pub(super) requires_tool_result_name: bool,
    pub(super) requires_assistant_after_tool_result: bool,
    pub(super) requires_thinking_as_text: bool,
    pub(super) requires_reasoning_content_on_assistant_messages: bool,
    pub(super) thinking_format: String,
    pub(super) chat_template_kwargs: Map<String, Value>,
    pub(super) chat_template_args: Map<String, Value>,
    pub(super) zai_tool_stream: bool,
    pub(super) supports_thinking_token_budget: bool,
    pub(super) thinking_token_budget_field: Option<String>,
    pub(super) supports_strict_mode: bool,
    pub(super) supports_openai_grammar_tools: bool,
    pub(super) supports_mid_convo_system_messages: bool,
    pub(super) supports_mid_convo_tool_additions: bool,
    pub(super) cache_control_format: Option<String>,
    pub(super) send_session_affinity_headers: bool,
    pub(super) session_affinity_format: String,
    pub(super) supports_long_cache_retention: bool,
    pub(super) vllm_priority: Option<Value>,
}

struct DetectedProviders {
    zai: bool,
    together: bool,
    moonshot: bool,
    openrouter: bool,
    workers_ai: bool,
    ai_gateway: bool,
    nvidia: bool,
    ant_ling: bool,
    cerebras: bool,
    deepseek: bool,
    grok: bool,
    chutes: bool,
    opencode: bool,
}

fn matches_provider(provider: &str, url: &str, names: &[&str], urls: &[&str]) -> bool {
    names.contains(&provider) || urls.iter().any(|needle| url.contains(needle))
}

impl DetectedProviders {
    fn detect(provider: &str, url: &str) -> Self {
        Self {
            zai: matches_provider(
                provider,
                url,
                &["zai", "zai-coding-cn"],
                &["api.z.ai", "open.bigmodel.cn"],
            ),
            together: matches_provider(provider, url, &["together"], &["api.together.ai", "api.together.xyz"]),
            moonshot: matches_provider(provider, url, &["moonshotai", "moonshotai-cn"], &["api.moonshot."]),
            openrouter: matches_provider(provider, url, &["openrouter"], &["openrouter.ai"]),
            workers_ai: matches_provider(provider, url, &["cloudflare-workers-ai"], &["api.cloudflare.com"]),
            ai_gateway: matches_provider(
                provider,
                url,
                &["cloudflare-ai-gateway"],
                &["gateway.ai.cloudflare.com"],
            ),
            nvidia: matches_provider(provider, url, &["nvidia"], &["integrate.api.nvidia.com"]),
            ant_ling: matches_provider(provider, url, &["ant-ling"], &["api.ant-ling.com"]),
            cerebras: matches_provider(provider, url, &["cerebras"], &["cerebras.ai"]),
            deepseek: provider == "deepseek" || url.to_lowercase().contains("deepseek.com"),
            grok: matches_provider(provider, url, &["xai"], &["api.x.ai"]),
            chutes: matches_provider(provider, url, &[], &["chutes.ai"]),
            opencode: matches_provider(provider, url, &["opencode"], &["opencode.ai"]),
        }
    }

    fn uses_max_tokens(&self) -> bool {
        [
            self.chutes,
            self.deepseek,
            self.moonshot,
            self.ai_gateway,
            self.together,
            self.nvidia,
            self.ant_ling,
            self.zai,
        ]
        .contains(&true)
    }

    fn is_non_standard(&self) -> bool {
        self.uses_max_tokens() || [self.cerebras, self.grok, self.opencode, self.workers_ai].contains(&true)
    }

    fn supports_reasoning_effort(&self) -> bool {
        ![
            self.grok,
            self.zai,
            self.moonshot,
            self.together,
            self.ai_gateway,
            self.nvidia,
            self.ant_ling,
        ]
        .contains(&true)
    }

    fn supports_long_cache_retention(&self) -> bool {
        ![
            self.together,
            self.workers_ai,
            self.ai_gateway,
            self.nvidia,
            self.ant_ling,
        ]
        .contains(&true)
    }

    fn thinking_format(&self) -> &'static str {
        if self.deepseek {
            return "deepseek";
        }
        if self.zai {
            return "zai";
        }
        if self.together {
            return "together";
        }
        if self.ant_ling {
            return "ant-ling";
        }
        if self.openrouter {
            return "openrouter";
        }
        "openai"
    }
}

fn compat_object(model: &Model, key: &str) -> Map<String, Value> {
    model
        .compat_value(key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn supports_developer_role_default(model: &Model, detected: &DetectedProviders, is_non_standard: bool) -> bool {
    let developer_role_model =
        detected.openrouter && (model.id.starts_with("anthropic/") || model.id.starts_with("openai/"));
    developer_role_model || (!is_non_standard && !detected.openrouter)
}

fn cache_control_format(model: &Model, provider: &str) -> Option<String> {
    model
        .compat_str("cacheControlFormat")
        .map(str::to_string)
        .or_else(|| (provider == "openrouter" && model.id.starts_with("anthropic/")).then(|| "anthropic".into()))
}

impl Compat {
    pub(super) fn of(model: &Model) -> Self {
        let provider = model.provider.as_str();
        let detected = DetectedProviders::detect(provider, model.base_url.as_str());
        let is_non_standard = detected.is_non_standard();
        let default_max_tokens_field = if detected.uses_max_tokens() {
            "max_tokens"
        } else {
            "max_completion_tokens"
        };
        let default_affinity_format = if detected.openrouter { "openrouter" } else { "openai" };
        Compat {
            supports_store: compat_flag(model, "supportsStore", !is_non_standard),
            supports_developer_role: compat_flag(
                model,
                "supportsDeveloperRole",
                supports_developer_role_default(model, &detected, is_non_standard),
            ),
            supports_reasoning_effort: compat_flag(
                model,
                "supportsReasoningEffort",
                detected.supports_reasoning_effort(),
            ),
            supports_usage_in_streaming: compat_flag(model, "supportsUsageInStreaming", true),
            supports_finish_reason: compat_flag(model, "supportsFinishReason", true),
            max_tokens_field: model
                .compat_str("maxTokensField")
                .unwrap_or(default_max_tokens_field)
                .to_string(),
            requires_tool_result_name: compat_flag(model, "requiresToolResultName", false),
            requires_assistant_after_tool_result: compat_flag(model, "requiresAssistantAfterToolResult", false),
            requires_thinking_as_text: compat_flag(model, "requiresThinkingAsText", false),
            requires_reasoning_content_on_assistant_messages: compat_flag(
                model,
                "requiresReasoningContentOnAssistantMessages",
                detected.deepseek,
            ),
            thinking_format: model
                .compat_str("thinkingFormat")
                .unwrap_or(detected.thinking_format())
                .to_string(),
            chat_template_kwargs: compat_object(model, "chatTemplateKwargs"),
            chat_template_args: compat_object(model, "chatTemplateArgs"),
            zai_tool_stream: compat_flag(model, "zaiToolStream", false),
            supports_thinking_token_budget: compat_flag(model, "supportsThinkingTokenBudget", false),
            thinking_token_budget_field: model.compat_str("thinkingTokenBudgetField").map(str::to_string),
            supports_strict_mode: compat_flag(model, "supportsStrictMode", false),
            supports_openai_grammar_tools: compat_flag(model, "supportsOpenAIGrammarTools", false),
            supports_mid_convo_system_messages: compat_flag(model, "supportsMidConvoSystemMessages", false),
            supports_mid_convo_tool_additions: compat_flag(model, "supportsMidConvoToolAdditions", false),
            cache_control_format: cache_control_format(model, provider),
            send_session_affinity_headers: compat_flag(model, "sendSessionAffinityHeaders", detected.openrouter),
            session_affinity_format: model
                .compat_str("sessionAffinityFormat")
                .unwrap_or(default_affinity_format)
                .to_string(),
            supports_long_cache_retention: compat_flag(
                model,
                "supportsLongCacheRetention",
                detected.supports_long_cache_retention(),
            ),
            vllm_priority: model.compat_value("vllmPriority").cloned(),
        }
    }
}
