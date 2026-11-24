//! Settings item catalog and direct toggle/cycle handlers for [`super::SettingsSelector`].

use super::*;

impl SettingsSelector {
    pub(super) fn items(&self) -> Vec<Item> {
        let c = &self.input;
        let b = |key: &str, fallback: bool| str_bool(c.bool(key, fallback));
        let mut items = vec![
            item(
                "autocompact",
                "Auto-compact",
                "Automatically compact context when it gets too large",
                b("compaction.enabled", true),
                &["true", "false"],
            ),
            item(
                "steering-mode",
                "Steering mode",
                "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
                c.str("steeringMode", config::keys::QUEUE_DEFAULT),
                &[config::keys::QUEUE_ONE_AT_A_TIME, config::keys::QUEUE_ALL],
            ),
            item(
                "follow-up-mode",
                "Follow-up mode",
                &format!(
                    "{} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
                    c.follow_up_key
                ),
                c.str("followUpMode", config::keys::QUEUE_DEFAULT),
                &[config::keys::QUEUE_ONE_AT_A_TIME, config::keys::QUEUE_ALL],
            ),
            item(
                "transport",
                "Transport",
                "Preferred transport for providers that support multiple transports",
                c.str("transport", "auto"),
                &config::keys::TRANSPORTS,
            ),
            item(
                "http-idle-timeout",
                "HTTP idle timeout",
                "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.",
                format_http_idle(c.num("httpIdleTimeoutMs", 300_000.0) as u64),
                &HTTP_IDLE.map(|(l, _)| l),
            ),
            item(
                "cache-warming-mode",
                "Cache warming",
                "off; streaming while the agent runs; idle also between runs while continuation stays profitable",
                c.str("cacheWarming", "streaming"),
                &config::keys::CACHE_WARMING_MODES,
            ),
            item(
                "hide-thinking",
                "Hide thinking",
                "Hide thinking blocks in assistant responses",
                b("hideThinkingBlock", false),
                &["true", "false"],
            ),
            item(
                "mermaid-rendering",
                "Mermaid diagrams",
                "Render Mermaid code blocks as Unicode diagrams",
                c.str("markdown.mermaid", "streaming"),
                &config::keys::MERMAID_MODES,
            ),
            item(
                "cache-miss-notices",
                "Cache miss notices",
                "Show transcript notices for cache costs and provider recovery diagnostics",
                b("showCacheMissNotices", false),
                &["true", "false"],
            ),
            item(
                "collapse-changelog",
                "Collapse changelog",
                "Show condensed changelog after updates",
                b("collapseChangelog", false),
                &["true", "false"],
            ),
            item(
                "quiet-startup",
                "Quiet startup",
                "Disable verbose printing at startup",
                b("quietStartup", false),
                &["true", "false"],
            ),
            item(
                "rlm-pane-auto-open",
                "RLM pane auto-open",
                "Open the RLM pane beside the chat when RLM work starts (wide terminals; the editor keeps the focus)",
                b("rlmPaneAutoOpen", true),
                &["true", "false"],
            ),
            item(
                "autoreview-enabled",
                "Autoreview",
                "Automatic pull-request reviews as your gh account (ultron autoreview run and its service; /autoreview on starts the service)",
                b("autoreview.enabled", true),
                &["true", "false"],
            ),
            item(
                "autoreview-mode",
                "Autoreview mode",
                "How autoreview runs: both, deep, fast, or auto (picks fast or deep from diff size and risk)",
                c.str("autoreview.mode", "both"),
                &["both", "deep", "fast", "auto"],
            ),
            item(
                "autoreview-quiet-minutes",
                "Autoreview quiet minutes",
                "Wait until a PR has no new updates for N minutes before starting a review (0 = immediate)",
                (c.num("autoreview.quietMinutes", 0.0) as u64).to_string(),
                &["0", "2", "5", "10", "15", "30"],
            ),
            item(
                "autoreview-never-request-changes",
                "Autoreview never request changes",
                "Always post as COMMENT instead of REQUEST_CHANGES, while still marking findings at blockAt or above as should fix",
                b("autoreview.neverRequestChanges", false),
                &["true", "false"],
            ),
            item(
                "autoreview-deep-rounds",
                "Autoreview deep rounds",
                "Maximum investigation rounds per deep tracer",
                (c.num("autoreview.deepRounds", 4.0) as u64).to_string(),
                &["2", "4", "6", "8", "10"],
            ),
            item(
                "autoreview-system-prefix",
                "Autoreview system prefix caching",
                "Send shared review context as the frame system prompt for prompt-cache reuse",
                b("autoreview.systemPrefix", false),
                &["true", "false"],
            ),
            item(
                "autoreview-ack",
                "Autoreview acknowledgement",
                "Post a short comment (an Ultron line) when a review starts",
                b("autoreview.ack", true),
                &["true", "false"],
            ),
            item(
                "self-review",
                "Self-review gate",
                "Self-review before shipping: advise, require (gates gh pr create), or off",
                c.str("selfReview", "advise"),
                &["advise", "require", "off"],
            ),
            item(
                "turn-end-checks",
                "Turn-end checks",
                "Run automatic end-of-turn checks when a turn finishes",
                b("turnEndChecks", true),
                &["true", "false"],
            ),
            item(
                "situational-skills",
                "Situational skills",
                "Enable automatic situational skill routing",
                b("situationalSkills", true),
                &["true", "false"],
            ),
            item(
                "observer-enabled",
                "Conversation observer",
                "Run background conversation observer checks every 6 model generations",
                b("observer.enabled", true),
                &["true", "false"],
            ),
            item(
                "loki-hygiene",
                "Loki code hygiene",
                "Code readability and hygiene advisories on added lines: on, all, or off",
                c.str("loki.hygiene", "on"),
                &["on", "all", "off"],
            ),
            {
                let art = c.str("autoreview.ackArt", "logo");
                let values: &[&str] = if art == "custom" {
                    &["custom", "logo", "none"]
                } else {
                    &["logo", "none"]
                };
                item(
                    "autoreview-ack-art",
                    "Autoreview acknowledgement art",
                    "Ultron's logo under the acknowledgement line, or none (custom text is set in settings.json)",
                    art,
                    values,
                )
            },
            item(
                "install-telemetry",
                "Install telemetry",
                "Send provider attribution headers (OpenRouter, NVIDIA NIM, Cloudflare); Ultron sends no install pings",
                b("enableInstallTelemetry", true),
                &["true", "false"],
            ),
            item(
                "default-project-trust",
                "Default project trust",
                "Fallback behavior when no extension or saved trust decision decides project trust",
                match c.str("defaultProjectTrust", "ask").as_str() {
                    "always" => "Always trust",
                    "never" => "Never trust",
                    _ => "Ask",
                }
                .to_string(),
                &["Ask", "Always trust", "Never trust"],
            ),
            item(
                "double-escape-action",
                "Double-escape action",
                "Action when pressing Escape twice with empty editor",
                c.str("doubleEscapeAction", "tree"),
                &["tree", "fork", "none"],
            ),
            item(
                "tree-filter-mode",
                "Tree filter mode",
                "Default filter when opening /tree",
                c.str("treeFilterMode", "default"),
                &config::keys::TREE_FILTER_MODES,
            ),
            Item {
                sub: Some(SubKind::Warnings),
                ..item(
                    "warnings",
                    "Warnings",
                    "Enable or disable individual warnings",
                    "configure".into(),
                    &[],
                )
            },
            Item {
                sub: Some(SubKind::ModelThinking),
                ..item(
                    "model-thinking",
                    "Default thinking level per model",
                    &format!(
                        "Override the default thinking level for specific models. {} cycles in-session.",
                        c.cycle_thinking_key
                    ),
                    self.thinking_summary(),
                    &[],
                )
            },
            item(
                "tui-mode",
                "TUI mode",
                "Interface layout; regular keeps the finished transcript in the terminal's scrollback",
                c.str("tuiMode", "fullscreen"),
                &config::keys::TUI_MODES,
            ),
            item(
                "fullscreen-exit-output",
                "Fullscreen exit output",
                "Print the transcript or only a session resume hint when exiting fullscreen mode",
                c.str("fullscreenExitOutput", "transcript"),
                &config::keys::FULLSCREEN_EXIT_OUTPUTS,
            ),
            item(
                "fullscreen-scrollbar",
                "Fullscreen scrollbar",
                "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
                c.str("fullscreenScrollbar", "auto"),
                &["auto", "always", "hidden"],
            ),
            item(
                "fullscreen-copy-on-select",
                "Fullscreen copy on select",
                "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
                b("fullscreenCopyOnSelect", true),
                &["true", "false"],
            ),
            Item {
                sub: Some(SubKind::Theme),
                ..item(
                    "theme",
                    "Theme",
                    "Color theme for the interface",
                    c.str("theme", "dark"),
                    &[],
                )
            },
        ];
        if c.rust_rows
            && let Some(at) = items.iter().position(|it| it.id == "autoreview-enabled")
        {
            items.insert(
                at,
                item(
                    "persona",
                    "Persona",
                    "The Ultron persona: personality.md in the system prompt, announced next steps continued (applies from the next prompt)",
                    if c.text(config::keys::PERSONA_KEY).as_deref() == Some(config::keys::PERSONA_ULTRON) {
                        config::keys::PERSONA_ULTRON
                    } else {
                        config::keys::PERSONA_OFF
                    }
                    .into(),
                    &config::keys::PERSONA_VALUES,
                ),
            );
            items.insert(
                at + 1,
                token_cap_item(
                    "request-token-cap",
                    "Request token cap",
                    "Tokens one request may spend, with its sub-agents and frames, before more model work is refused (applies to new sessions; ULTRON_MAX_TOTAL_TOKENS overrides it)",
                    c.get("rootBudget.maxTotalTokens"),
                    &REQUEST_TOKEN_CAPS,
                ),
            );
            items.insert(
                at + 2,
                token_cap_item(
                    "map-token-cap",
                    "rlm.map token cap",
                    "Tokens an rlm.map that sets no budget may spend; frames past it come back Incomplete (applies to the next map; ULTRON_RLM_MAP_TOKENS overrides it)",
                    c.get("rlm.mapTokens"),
                    &MAP_TOKEN_CAPS,
                ),
            );
            items.insert(
                at + 3,
                item(
                    "judge-provider",
                    "Judge backends",
                    "Who answers judge.decide, in order: Jev (TypeSafe key), d1 (judge.d1.url and key); `ultron setup` sets them up. Models: the judge's fallback and escalation models.",
                    judge_order(c.get("judge.provider")),
                    &JUDGE_ORDERS,
                ),
            );
            let budget = budget_label(c.get("judge.budgetUsd"));
            let mut budgets = JUDGE_BUDGETS.to_vec();
            if !budgets.contains(&budget.as_str()) {
                budgets.insert(1, &budget);
            }
            items.insert(
                at + 4,
                item(
                    "judge-budget",
                    "Judge budget",
                    "USD the judge may spend in a session (Jev and the fallback and escalation models; d1 is free). Off: no cap, spend is still recorded.",
                    budget.clone(),
                    &budgets,
                ),
            );
            items.insert(
                at + 5,
                item(
                    "memory-backend",
                    "Memory backend",
                    "Where memory lives (applies to new sessions). detect: memorantado once it answers, kept from then on; until then Hindsight. Once memorantado is the backend, an outage pauses memory instead of writing to Hindsight.",
                    c.text("memory.backend")
                        .filter(|b| MEMORY_BACKENDS.contains(&b.as_str()))
                        .unwrap_or_else(|| MEMORY_BACKENDS[0].into()),
                    &MEMORY_BACKENDS,
                ),
            );
            items.insert(
                at + 6,
                item(
                    "result-footer",
                    "REPL result footer",
                    "End each rlm result with [t+<time since the session started> ctx <tokens>], and tell the model so (applies to new sessions)",
                    b("rlm.resultFooter", false),
                    &["true", "false"],
                ),
            );
        }
        let mut at = 1;
        if c.supports_images {
            items.insert(
                1,
                item(
                    "show-images",
                    "Show images",
                    "Render images inline in terminal",
                    b("terminal.showImages", true),
                    &["true", "false"],
                ),
            );
            items.insert(
                2,
                item(
                    "image-width-cells",
                    "Image width",
                    "Preferred inline image width in terminal cells",
                    (c.num("terminal.imageWidthCells", 60.0) as i64).to_string(),
                    &["60", "80", "120"],
                ),
            );
            at = 3;
        }
        let tail = [
            item(
                "auto-resize-images",
                "Auto-resize images",
                "Resize large images to 2000x2000 max for better model compatibility",
                b("images.autoResize", true),
                &["true", "false"],
            ),
            item(
                "block-images",
                "Block images",
                "Prevent images from being sent to LLM providers",
                b("images.blockImages", false),
                &["true", "false"],
            ),
            item(
                "skill-commands",
                "Skill commands",
                "Register skills as /skill:name commands",
                b("enableSkillCommands", true),
                &["true", "false"],
            ),
            item(
                "show-hardware-cursor",
                "Show hardware cursor",
                "Show the terminal cursor while still positioning it for IME support",
                b("showHardwareCursor", false),
                &["true", "false"],
            ),
            item(
                "editor-padding",
                "Editor padding",
                "Horizontal padding for input editor (0-3)",
                (c.num("editorPaddingX", 0.0) as i64).to_string(),
                &["0", "1", "2", "3"],
            ),
            item(
                "output-padding",
                "Output padding",
                "Horizontal padding for user messages, assistant messages, and thinking",
                if c.num("outputPad", 1.0) == 0.0 { "0" } else { "1" }.into(),
                &["0", "1"],
            ),
            item(
                "autocomplete-max-visible",
                "Autocomplete max items",
                "Max visible items in autocomplete dropdown (3-20)",
                (c.num("autocompleteMaxVisible", 5.0) as i64).to_string(),
                &["3", "5", "7", "10", "15", "20"],
            ),
            item(
                "clear-on-shrink",
                "Clear on shrink",
                "Clear empty rows when content shrinks (may cause flicker)",
                b("terminal.clearOnShrink", false),
                &["true", "false"],
            ),
            item(
                "terminal-progress",
                "Terminal progress",
                "Show OSC 9;4 progress indicators in the terminal tab bar",
                b("terminal.showTerminalProgress", false),
                &["true", "false"],
            ),
        ];
        for (i, it) in tail.into_iter().enumerate() {
            items.insert(at + i, it);
        }
        items.insert(
            0,
            Item {
                sub: Some(SubKind::Models),
                ..item(
                    "models",
                    "Models",
                    "Models of the session, inference frames, /review and sub-agents, and of Claude Code mode. Changes apply to the running session.",
                    self.models_summary(),
                    &[],
                )
            },
        );
        items
    }

    pub(super) fn main_change(&mut self, id: &str, value: &str) -> SettingsEvent {
        let b = Value::Bool(value == "true");
        let n = |v: &str| Value::from(v.parse::<i64>().unwrap_or(0));
        let (key, v) = match id {
            "autocompact" => ("compaction.enabled", b),
            "show-images" => ("terminal.showImages", b),
            "image-width-cells" => ("terminal.imageWidthCells", n(value)),
            "auto-resize-images" => ("images.autoResize", b),
            "block-images" => ("images.blockImages", b),
            "skill-commands" => ("enableSkillCommands", b),
            "steering-mode" => ("steeringMode", value.into()),
            "follow-up-mode" => ("followUpMode", value.into()),
            "transport" => ("transport", value.into()),
            "http-idle-timeout" => (
                "httpIdleTimeoutMs",
                Value::from(HTTP_IDLE.iter().find(|(l, _)| *l == value).map_or(300_000, |(_, v)| *v)),
            ),
            "cache-warming-mode" => ("cacheWarming", value.into()),
            "hide-thinking" => ("hideThinkingBlock", b),
            "mermaid-rendering" => ("markdown.mermaid", value.into()),
            "cache-miss-notices" => ("showCacheMissNotices", b),
            "collapse-changelog" => ("collapseChangelog", b),
            "quiet-startup" => ("quietStartup", b),
            "rlm-pane-auto-open" => ("rlmPaneAutoOpen", b),
            "autoreview-enabled" => ("autoreview.enabled", b),
            "autoreview-mode" => ("autoreview.mode", value.into()),
            "autoreview-quiet-minutes" => ("autoreview.quietMinutes", n(value)),
            "autoreview-never-request-changes" => ("autoreview.neverRequestChanges", b),
            "autoreview-deep-rounds" => ("autoreview.deepRounds", n(value)),
            "autoreview-system-prefix" => ("autoreview.systemPrefix", b),
            "self-review" => ("selfReview", value.into()),
            "turn-end-checks" => ("turnEndChecks", b),
            "situational-skills" => ("situationalSkills", b),
            "observer-enabled" => ("observer.enabled", b),
            "loki-hygiene" => ("loki.hygiene", value.into()),
            "autoreview-ack" => ("autoreview.ack", b),
            "autoreview-ack-art" => ("autoreview.ackArt", value.into()),
            "install-telemetry" => ("enableInstallTelemetry", b),
            "default-project-trust" => (
                "defaultProjectTrust",
                match value {
                    "Always trust" => "always",
                    "Never trust" => "never",
                    _ => "ask",
                }
                .into(),
            ),
            "double-escape-action" => ("doubleEscapeAction", value.into()),
            "tree-filter-mode" => ("treeFilterMode", value.into()),
            "show-hardware-cursor" => ("showHardwareCursor", b),
            "editor-padding" => ("editorPaddingX", n(value)),
            "output-padding" => ("outputPad", n(value)),
            "autocomplete-max-visible" => ("autocompleteMaxVisible", n(value)),
            "clear-on-shrink" => ("terminal.clearOnShrink", b),
            "terminal-progress" => ("terminal.showTerminalProgress", b),
            "tui-mode" => ("tuiMode", value.into()),
            "fullscreen-exit-output" => ("fullscreenExitOutput", value.into()),
            "fullscreen-scrollbar" => ("fullscreenScrollbar", value.into()),
            "fullscreen-copy-on-select" => ("fullscreenCopyOnSelect", b),
            "persona" => (
                config::keys::PERSONA_KEY,
                if value == config::keys::PERSONA_ULTRON {
                    value
                } else {
                    config::keys::PERSONA_OFF
                }
                .into(),
            ),
            "request-token-cap" => ("rootBudget.maxTotalTokens", token_cap_value(value)),
            "map-token-cap" => ("rlm.mapTokens", token_cap_value(value)),
            "result-footer" => ("rlm.resultFooter", b),
            "judge-provider" => ("judge.provider", judge_order_value(value)),
            "judge-budget" => ("judge.budgetUsd", budget_value(value)),
            "memory-backend" => (
                "memory.backend",
                if value == MEMORY_BACKENDS[0] {
                    Value::Null
                } else {
                    value.into()
                },
            ),
            other => (other, value.into()),
        };
        set_path(&mut self.input.settings, key, v.clone());
        SettingsEvent::Change(key.to_string(), v)
    }
}
