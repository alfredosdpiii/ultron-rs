//! Models, theme, and thinking-level submenus for [`super::SettingsSelector`].

use super::*;

type ModelChoice = (String, String, Option<String>, Option<String>);

#[derive(Clone)]
pub(super) struct ModelRow {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) description: String,
    pub(super) display: String,
    pub(super) locked: Option<String>,
    pub(super) heading: bool,
    /// `(value, label, description, group)`.
    pub(super) choices: Vec<ModelChoice>,
    pub(super) current: String,
}

impl SettingsSelector {
    pub(super) fn thinking_summary(&self) -> String {
        match self.model_thinking.len() {
            0 => "none".into(),
            n => format!("{n} configured"),
        }
    }

    pub(super) fn model_rows(&self) -> Vec<ModelRow> {
        let c = &self.input;
        let choice = |m: &ModelInfo| {
            (
                model_ref(m),
                m.id.clone(),
                (!m.name.is_empty() && m.name != m.id).then(|| m.name.clone()),
                Some(m.provider.clone()),
            )
        };
        let all: Vec<_> = c.models.iter().map(choice).collect();
        let claude: Vec<_> = c
            .models
            .iter()
            .filter(|m| m.provider == CLAUDE_CODE)
            .map(choice)
            .collect();
        let session = c.session.as_ref().map_or("none".to_string(), model_ref);
        let plain = |v: &str, l: &str| (v.to_string(), l.to_string(), None, None);
        let with = |head: ModelChoice, rest: &[ModelChoice]| {
            std::iter::once(head).chain(rest.iter().cloned()).collect::<Vec<_>>()
        };
        let row =
            |id: &str, label: &str, description: &str, display: String, choices: Vec<_>, current: Option<String>| {
                let env = c.env_overrides.get(id);
                ModelRow {
                    id: id.into(),
                    label: label.into(),
                    description: description.into(),
                    display: env.map_or(display, |(name, value)| format!("{value} (set by {name})")),
                    locked: env.map(|(name, _)| {
                        format!("Set by {name}; unset it to choose here (the variable wins over the setting).")
                    }),
                    heading: false,
                    choices,
                    current: current.unwrap_or_default(),
                }
            };
        let frame = c.text("rlm.frameModel");
        let effective_frame = c
            .env_overrides
            .get("rlm.frameModel")
            .map(|e| e.1.clone())
            .or(frame.clone());
        let review = c.text("review.model");
        let child = c.text("rlm.childModel");
        let frame_thinking = c.text("rlm.frameThinking");
        let cc_root = c.text("claudeCode.model");
        let cc_frame = c.text("claudeCode.frameModel");
        let cc_child = c.text("claudeCode.childModel");
        let cc_ultron = c.text("claudeCode.ultronChildModel");
        let saved_default = match (c.text("defaultProvider"), c.text("defaultModel")) {
            (Some(p), Some(m)) => Some(format!("{p}/{m}")),
            _ => None,
        };
        let alias = |v: &Option<String>| v.as_ref().map(|v| format!("{CLAUDE_CODE}/{v}"));
        let setting = |v: &Option<String>, fallback: String| v.as_ref().map_or(fallback, |v| format!("{v} (setting)"));
        let durable = c.get("durable").is_object().then(|| c.text("durable.model"));
        let durable_default = CLAUDE_CODE_DEFAULT_REF.to_string();
        let session_saved = match &durable {
            Some(model) => format!(
                "{} (durable.model)",
                model.clone().unwrap_or_else(|| durable_default.clone())
            ),
            None => saved_default.unwrap_or_else(|| "none".into()),
        };
        let mut rows = vec![
            row(
                "session",
                "Session model",
                &format!(
                    "The Session's own model. Choosing one switches the running Session now and saves it as the default, as /model does. Saved default: {session_saved}."
                ),
                format!("{session} (session)"),
                all.clone(),
                c.session.as_ref().map(model_ref),
            ),
            row(
                "rlm.frameModel",
                "Frame model",
                "Code-free rlm.infer / rlm.map frames that name no model= (and /review frames without a review model). Order: model= > ULTRON_RLM_FRAME_MODEL > this setting > the session model.",
                setting(&frame, format!("same as session ({session})")),
                with(plain("", "Same as session model"), &all),
                frame.clone(),
            ),
            row(
                "review.model",
                "Review model",
                "/review frames. Order: --model > ULTRON_REVIEW_MODEL > this setting > the frame model.",
                match (&review, &effective_frame) {
                    (Some(r), _) => format!("{r} (setting)"),
                    (None, Some(f)) => format!("{f} (frame model)"),
                    _ => format!("same as session ({session})"),
                },
                with(plain("", "Same as frame model"), &all),
                review.clone(),
            ),
            row(
                "rlm.childModel",
                "Sub-agent model",
                "rlm.spawn sub-agents that name no model= (claude-code models run on the Claude subscription).",
                setting(&child, format!("same as session ({session})")),
                with(plain("", "Same as session model"), &all),
                child.clone(),
            ),
            row(
                RACE_MODELS,
                "Race models",
                "Models that subagent races (arena, swarm model races) run on, one arm each. Pick to toggle; none: the skills choose.",
                match c.race_models() {
                    picked if picked.is_empty() => "none (skills choose)".into(),
                    picked => picked.join(", "),
                },
                with(plain("", "Clear"), &all),
                None,
            ),
            row(
                "rlm.frameThinking",
                "Frame thinking",
                "Thinking level of inference frames (claude-code frames pass it as --effort).",
                setting(&frame_thinking, "same as session".into()),
                with(
                    plain("", "Same as session"),
                    &["off", "low", "medium", "high"].map(|l| plain(l, l)),
                ),
                frame_thinking.clone(),
            ),
            ModelRow {
                id: "claude-code-heading".into(),
                label: "Claude Code mode".into(),
                description: "`ultron claude`: Claude Code as the root agent. These apply when it next starts.".into(),
                display: String::new(),
                locked: None,
                heading: true,
                choices: Vec::new(),
                current: String::new(),
            },
            row(
                "claudeCode.model",
                "Claude Code root model",
                "The model Claude Code itself runs (claude --model); ULTRON_CLAUDE_MODEL overrides it.",
                setting(&cc_root, format!("{CLAUDE_CODE_DEFAULT_MODEL} (default)")),
                with(plain("", &format!("Default ({CLAUDE_CODE_DEFAULT_MODEL})")), &claude),
                alias(&cc_root),
            ),
            row(
                "claudeCode.frameModel",
                "Claude Code frame model",
                "rlm.map / rlm.infer frames under `ultron claude`; ULTRON_CLAUDE_FRAME_MODEL overrides it.",
                setting(&cc_frame, format!("{CLAUDE_CODE_DEFAULT_REF} (default)")),
                with(plain("", &format!("Default ({CLAUDE_CODE_DEFAULT_REF})")), &all),
                cc_frame.clone(),
            ),
            row(
                "claudeCode.childModel",
                "Claude Code sub-agent model",
                "Model of Claude Code sub-agents (children=claude); ULTRON_CLAUDE_CHILD_MODEL overrides it.",
                setting(&cc_child, format!("{CLAUDE_CODE_DEFAULT_MODEL} (default)")),
                with(plain("", &format!("Default ({CLAUDE_CODE_DEFAULT_MODEL})")), &claude),
                alias(&cc_child),
            ),
            row(
                "claudeCode.ultronChildModel",
                "Ultron sub-agent model",
                "Model of Ultron sub-agents under `ultron claude` (children=ultron); unset, they run on the root's Claude subscription model.",
                setting(&cc_ultron, "root's subscription model".into()),
                with(plain("", "Root's subscription model"), &all),
                cc_ultron.clone(),
            ),
        ];
        if c.rust_rows
            && let Some(at) = rows.iter().position(|r| r.id == "claude-code-heading")
        {
            let observer = c.text("observer.model");
            rows.insert(
                at,
                row(
                    "observer.model",
                    "Observer model",
                    "Conversation observer checks. Unset uses the current session model. Changes apply to the next check; the session model is unchanged.",
                    setting(&observer, format!("same as session ({session})")),
                    with(plain("", "Same as session model"), &all),
                    observer,
                ),
            );
            let fallback = c.text("judge.fallbackModel");
            let escalate = c.text("judge.escalateModel");
            rows.insert(
                at,
                row(
                    "judge.fallbackModel",
                    "Judge fallback model",
                    "judge.decide when neither Jev nor d1 answers; none: the verdict abstains.",
                    setting(&fallback, "none".into()),
                    with(plain("", "None"), &all),
                    fallback.clone(),
                ),
            );
            rows.insert(
                at + 1,
                row(
                    "judge.escalateModel",
                    "Judge escalation model",
                    "judge.decide answers under their confidence threshold go to it; it must be of another family than the model that answered. Default: the cheapest model of the session model's family.",
                    setting(&escalate, "cheapest of the session's family".into()),
                    with(plain("", "Default (cheapest of the session's family)"), &all),
                    escalate.clone(),
                ),
            );
        }
        if let Some(model) = durable {
            rows.insert(
                1,
                row(
                    "durable.model",
                    "Durable default model",
                    "The model new sessions of the durable instance (ultron durable, ultron-s) start on when no --model names one, ahead of the default model; /model and Ctrl+P save here. claude-code models run tool lanes on the Claude subscription.",
                    setting(&model, format!("{durable_default} (default)")),
                    with(plain("", &format!("Default ({durable_default})")), &all),
                    model.clone(),
                ),
            );
        }
        rows
    }

    pub(super) fn models_summary(&self) -> String {
        let rows = self.model_rows();
        let frame = rows
            .iter()
            .find(|r| r.id == "rlm.frameModel")
            .map(|r| r.display.clone());
        format!("frames: {}", frame.unwrap_or_default())
    }

    pub(super) fn models_list(&self) -> (List, Vec<String>) {
        let t = &self.theme;
        let items = self
            .model_rows()
            .into_iter()
            .map(|row| Item {
                id: row.id.clone(),
                label: if row.heading {
                    bold(&row.label)
                } else {
                    row.label.clone()
                },
                description: Some(match &row.locked {
                    None => row.description.clone(),
                    Some(l) => format!("{} {l}", row.description),
                }),
                value: match &row.locked {
                    None => row.display.clone(),
                    Some(_) => format!("{} · locked", row.display),
                },
                values: Vec::new(),
                sub: (!row.heading && row.locked.is_none() && !row.choices.is_empty())
                    .then(|| SubKind::ModelPick(row.id.clone())),
            })
            .collect::<Vec<_>>();
        let n = items.len().min(14);
        let header = vec![
            bold(&t.fg(Fg::Accent, "Models")),
            t.fg(Fg::Muted,
                "Changes apply to the running session: the next frame, review or sub-agent uses them. Claude Code mode rows apply when `ultron claude` next starts.",
            ),
            String::new(),
        ];
        (List::new(Rc::clone(t), self.keys.clone(), items, n, false), header)
    }

    pub(super) fn picker_items(&self, row: &ModelRow) -> Vec<SelectItem> {
        let race = (row.id == RACE_MODELS).then(|| self.input.race_models());
        let on = |v: &String| race.as_ref().map_or(*v == row.current, |r| r.contains(v));
        let mark = |(v, l, d, _): &ModelChoice| SelectItem {
            value: v.clone(),
            label: format!("{}{l}", if on(v) { "✓ " } else { "  " }),
            description: d.clone(),
        };
        let mut items: Vec<SelectItem> = row.choices.iter().filter(|c| c.3.is_none()).map(mark).collect();
        let mut groups: BTreeMap<String, Vec<&ModelChoice>> = BTreeMap::new();
        for c in row.choices.iter().filter(|c| c.3.is_some()) {
            groups.entry(c.3.clone().unwrap_or_default()).or_default().push(c);
        }
        for (group, mut list) in groups {
            items.push(SelectItem {
                value: format!("{GROUP_PREFIX}{group}"),
                label: self.theme.fg(Fg::Muted, &format!("── {group} ──")),
                description: None,
            });
            list.sort_by(|a, b| a.1.cmp(&b.1));
            items.extend(list.into_iter().map(mark));
        }
        items
    }

    pub(super) fn open(&mut self, kind: SubKind) {
        let t = Rc::clone(&self.theme);
        let keys = self.keys.clone();
        let sub = match &kind {
            SubKind::Warnings => {
                let items = vec![item(
                    "anthropic-extra-usage",
                    "Anthropic extra usage",
                    "Warn when Anthropic subscription auth may use paid extra usage",
                    str_bool(self.warnings_extra),
                    &["true", "false"],
                )];
                Sub::List {
                    kind,
                    list: List::new(t, keys, items, 1, false),
                    header: Vec::new(),
                }
            }
            SubKind::Theme => self.theme_single(),
            SubKind::LightTheme | SubKind::DarkTheme => {
                let (title, description, current) = if matches!(kind, SubKind::LightTheme) {
                    (
                        "Light Theme",
                        "Select the theme to use for light terminal appearance",
                        self.light.clone(),
                    )
                } else {
                    (
                        "Dark Theme",
                        "Select the theme to use for dark terminal appearance",
                        self.dark.clone(),
                    )
                };
                Sub::Select {
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (title, description),
                        theme_items(&self.input.themes, &current),
                        &current,
                        false,
                        (12, 32),
                    ),
                    kind,
                }
            }
            SubKind::ModelThinking => self.thinking_step(),
            SubKind::Models => {
                let (list, header) = self.models_list();
                Sub::List { kind, list, header }
            }
            SubKind::ModelPick(id) => {
                let Some(row) = self.model_rows().into_iter().find(|r| r.id == *id) else {
                    return;
                };
                Sub::Select {
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (&row.label, &row.description),
                        self.picker_items(&row),
                        &row.current,
                        true,
                        (12, 46),
                    ),
                    kind,
                }
            }
        };
        self.stack.push(sub);
    }

    pub(super) fn theme_single(&self) -> Sub {
        let setting = self.input.str("theme", "dark");
        let fixed = (!setting.contains('/')).then_some(setting.as_str());
        let current = preferred(&self.input.themes, fixed, "dark");
        let mut items = vec![SelectItem {
            value: AUTOMATIC_THEME.into(),
            label: "  Automatic".into(),
            description: Some("Use separate themes for light and dark terminal appearance".into()),
        }];
        items.extend(theme_items(&self.input.themes, &current));
        Sub::Select {
            kind: SubKind::Theme,
            menu: SelectSubmenu::new(
                Rc::clone(&self.theme),
                self.keys.clone(),
                (
                    "Theme",
                    "Select a theme, or choose Automatic to follow terminal appearance.",
                ),
                items,
                &current,
                false,
                (12, 32),
            ),
        }
    }

    pub(super) fn theme_automatic(&self) -> Sub {
        let t = &self.theme;
        let items = vec![
            Item {
                sub: Some(SubKind::LightTheme),
                ..item(
                    "light-theme",
                    "Light theme",
                    "Theme to use in automatic mode when the terminal is light",
                    self.light.clone(),
                    &[],
                )
            },
            Item {
                sub: Some(SubKind::DarkTheme),
                ..item(
                    "dark-theme",
                    "Dark theme",
                    "Theme to use in automatic mode when the terminal is dark",
                    self.dark.clone(),
                    &[],
                )
            },
            item(
                "apply",
                "Apply",
                "Save and go back",
                "save and go back".into(),
                &["save and go back"],
            ),
            item(
                "single-mode",
                "Change mode",
                "Switch to one theme for light and dark",
                "switch to single theme".into(),
                &["switch to single theme"],
            ),
        ];
        Sub::List {
            kind: SubKind::Theme,
            list: List::new(Rc::clone(t), self.keys.clone(), items, 4, false),
            header: vec![
                bold(&t.fg(Fg::Accent, "Automatic Theme")),
                String::new(),
                t.fg(Fg::Muted, "Choose themes for terminal light and dark appearance."),
                t.fg(Fg::Muted, "Light/dark detection requires terminal support."),
                String::new(),
            ],
        }
    }

    pub(super) fn thinking_step(&self) -> Sub {
        let t = Rc::clone(&self.theme);
        let keys = self.keys.clone();
        let current_key = self.input.session.as_ref().map(model_ref);
        let default_key = match (self.input.text("defaultProvider"), self.input.text("defaultModel")) {
            (Some(p), Some(m)) => Some(format!("{p}/{m}")),
            _ => None,
        };
        match &self.thinking_model {
            None => {
                let mut models: Vec<&ModelInfo> = self.input.models.iter().collect();
                let rank = |m: &ModelInfo| {
                    let key = model_ref(m);
                    if Some(&key) == current_key.as_ref() {
                        0
                    } else if Some(&key) == default_key.as_ref() {
                        1
                    } else {
                        2
                    }
                };
                models.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.provider.cmp(&b.provider)));
                let mut items: Vec<SelectItem> = models
                    .iter()
                    .map(|m| SelectItem {
                        value: model_ref(m),
                        label: format!("{} {}", m.id, t.fg(Fg::Muted, &format!("[{}]", m.provider))),
                        description: self.model_thinking.get(&model_ref(m)).cloned(),
                    })
                    .collect();
                if items.is_empty() {
                    items.push(SelectItem {
                        value: "__none__".into(),
                        label: "No models available".into(),
                        description: Some("Log in to a provider or configure an API key first".into()),
                    });
                }
                let preselect = current_key.or(default_key).unwrap_or_default();
                Sub::Select {
                    kind: SubKind::ModelThinking,
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        ("Per-Model Thinking Level", "Step 1/2 · Select a model to configure"),
                        items,
                        &preselect,
                        true,
                        (12, 46),
                    ),
                }
            }
            Some(key) => {
                let model = self.input.models.iter().find(|m| model_ref(m) == *key);
                let levels: Vec<String> = match model {
                    Some(m) if m.reasoning => m.levels(),
                    _ => vec!["off".into()],
                };
                let active = self.model_thinking.get(key).cloned();
                let mut items: Vec<SelectItem> = levels
                    .iter()
                    .map(|l| SelectItem {
                        value: l.clone(),
                        label: format!("{}{l}", if Some(l) == active.as_ref() { "✓ " } else { "  " }),
                        description: Some(thinking_description(l).into()),
                    })
                    .collect();
                if active.is_some() {
                    items.push(SelectItem {
                        value: CLEAR_OVERRIDE.into(),
                        label: "  (clear override)".into(),
                        description: Some(format!(
                            "Revert to global default ({})",
                            self.input.str("defaultThinkingLevel", "medium")
                        )),
                    });
                }
                let title = match model {
                    Some(m) => format!("Thinking Level for {} [{}]", m.id, m.provider),
                    None => format!("Thinking Level for {key}"),
                };
                Sub::Select {
                    kind: SubKind::ModelThinking,
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (&title, "Step 2/2 · Select default thinking level for this model"),
                        items,
                        active.as_deref().unwrap_or(""),
                        false,
                        (12, 32),
                    ),
                }
            }
        }
    }
}

pub(super) fn set_path(settings: &mut Value, key: &str, value: Value) {
    let mut v = settings;
    let parts: Vec<&str> = key.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        if !v[*part].is_object() {
            v[*part] = json!({});
        }
        v = &mut v[*part];
    }
    v[parts[parts.len() - 1]] = value;
}

pub(super) fn theme_items(themes: &[String], current: &str) -> Vec<SelectItem> {
    themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: format!("{}{name}", if name == current { "✓ " } else { "  " }),
            description: None,
        })
        .collect()
}

pub(super) fn preferred(themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    match preferred {
        Some(p) if themes.iter().any(|t| t == p) => p.to_string(),
        _ if themes.iter().any(|t| t == fallback) => fallback.to_string(),
        _ => themes.first().cloned().unwrap_or_else(|| fallback.to_string()),
    }
}

/// `defaultAutomaticThemes`: `light/dark` from an automatic setting, else the fixed theme for both.
pub(super) fn automatic_themes(setting: &str, themes: &[String]) -> (String, String) {
    if let Some((light, dark)) = setting.split_once('/')
        && !light.is_empty()
        && !dark.is_empty()
    {
        return (light.to_string(), dark.to_string());
    }
    let fixed = (!setting.contains('/')).then_some(setting);
    let name = preferred(themes, fixed, "dark");
    (name.clone(), name)
}
