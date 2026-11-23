use super::{Config, Env};
use jsrt::sync;
use std::sync::{Arc, Mutex};

/// The root system prompt, rendered again only when the settings' [`crate::lane::PromptMode`] changes (engineering
/// mode, `alwaysSkills`), so the prompt-cache prefix stays stable (session-worker.ts `currentSystemPrompt`). A
/// change applies from the next request.
#[derive(Clone)]
pub struct SystemPrompt(Arc<PromptState>);

type RenderPrompt = dyn Fn(&crate::lane::PromptMode) -> Result<(String, Vec<String>), String> + Send + Sync;
type RenderChildPrompt = dyn Fn(&crate::lane::PromptMode, ChildReader) -> Result<String, String> + Send + Sync;

/// Who a subagent's prompt is for: an in-memory child (without the durable runtime's APIs) or not, and how much its
/// guide says about memory (crate::memory_guidance, by its model's tier).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildReader {
    pub in_memory: bool,
    pub memory: crate::memory_guidance::MemoryGuidance,
}

struct PromptState {
    /// `None`: a fixed prompt.
    render: Option<Box<RenderPrompt>>,
    /// A subagent's prompt, by [`ChildReader`]; `None`: as the root's.
    render_child: Option<Box<RenderChildPrompt>>,
    /// The subagent prompt last rendered, by mode and reader.
    child: Mutex<Option<(crate::lane::PromptMode, ChildReader, String)>>,
    mode: Box<dyn Fn() -> crate::lane::PromptMode + Send + Sync>,
    current: Mutex<(crate::lane::PromptMode, String, Vec<String>)>,
    /// The session's own persona toggle over the settings' (resources::keep_going): `None` follows the settings.
    persona: Mutex<Option<bool>>,
    /// The root lane's model id ([`crate::lane::PromptMode::model`]).
    model: Mutex<String>,
}

impl SystemPrompt {
    pub fn fixed(text: impl Into<String>) -> SystemPrompt {
        SystemPrompt(Arc::new(PromptState {
            render: None,
            render_child: None,
            child: Mutex::new(None),
            mode: Box::new(crate::lane::PromptMode::default),
            current: Mutex::new((Default::default(), text.into(), Vec::new())),
            persona: Mutex::new(None),
            model: Mutex::new(String::new()),
        }))
    }

    /// The native worker's prompt for `env`'s working directory, in the mode its settings name now (read with the
    /// worker's project trust).
    pub(super) fn rendered(
        env: &Env,
        config: &Config,
        trusted: bool,
        guide: resources::runtime_guide::RuntimeGuideOptions,
        flags: &crate::session_flags::SessionFlags,
        extra: Arc<crate::session_resources::SessionResources>,
    ) -> Result<SystemPrompt, String> {
        let (cwd, agent_dir) = (env.cwd.clone(), config.agent_dir.clone());
        let (no_skills, no_context_files) = (flags.no_skills, flags.no_context_files);
        let mode = move || {
            let settings = config::settings::SettingsManager::create(&cwd, &agent_dir, trusted);
            crate::lane::PromptMode {
                no_skills,
                no_context_files,
                ..crate::lane::PromptMode::from_settings(&settings)
            }
        };
        let (env, loki_root) = (env.clone(), config.loki_root.clone());
        let (child_env, child_loki, child_extra) = (env.clone(), loki_root.clone(), extra.clone());
        let child_guide = guide.clone();
        let render = move |mode: &crate::lane::PromptMode| {
            crate::lane::system_prompt_with_resources(
                &env,
                &env.cwd.display().to_string(),
                loki_root.as_deref(),
                &guide,
                mode,
                &extra,
            )
        };
        let render_child = move |mode: &crate::lane::PromptMode, reader: ChildReader| {
            let guide = resources::runtime_guide::RuntimeGuideOptions {
                reader: resources::runtime_guide::GuideReader::Subagent {
                    swarm: !reader.in_memory,
                },
                memory: reader.memory,
                ..child_guide.clone()
            };
            crate::lane::system_prompt_with_resources(
                &child_env,
                &child_env.cwd.display().to_string(),
                child_loki.as_deref(),
                &guide,
                mode,
                &child_extra,
            )
            .map(|(text, _)| text)
        };
        let initial = mode();
        let (text, warnings) = render(&initial)?;
        Ok(SystemPrompt(Arc::new(PromptState {
            render: Some(Box::new(render)),
            render_child: Some(Box::new(render_child)),
            child: Mutex::new(None),
            mode: Box::new(mode),
            current: Mutex::new((initial, text, warnings)),
            persona: Mutex::new(None),
            model: Mutex::new(String::new()),
        })))
    }

    /// The prompt in the settings' current mode. A mode whose prompt does not render keeps the last one.
    pub fn current(&self) -> String {
        let state = &self.0;
        let Some(render) = &state.render else {
            return sync::lock(&state.current).1.clone();
        };
        let mut mode = (state.mode)();
        if let Some(persona) = *sync::lock(&state.persona) {
            mode.persona = persona;
        }
        mode.model = sync::lock(&state.model).clone();
        let mut current = sync::lock(&state.current);
        if current.0 != mode
            && let Ok((text, warnings)) = render(&mode)
        {
            *current = (mode, text, warnings);
        }
        current.1.clone()
    }

    /// A subagent's prompt in the settings' current mode, for `reader`: the root's, with the subagent's runtime
    /// guide and the notes of `model` (the subagent's own). A fixed prompt is the same for every lane.
    pub fn child_current(&self, reader: ChildReader, model: &str) -> String {
        let state = &self.0;
        let Some(render) = &state.render_child else {
            return self.current();
        };
        let mut mode = (state.mode)();
        if let Some(persona) = *sync::lock(&state.persona) {
            mode.persona = persona;
        }
        mode.model = model.to_string();
        let mut child = sync::lock(&state.child);
        if let Some((cached, cached_reader, text)) = &*child
            && *cached == mode
            && *cached_reader == reader
        {
            return text.clone();
        }
        match render(&mode, reader) {
            Ok(text) => {
                *child = Some((mode, reader, text.clone()));
                text
            }
            Err(_) => {
                drop(child);
                self.current()
            }
        }
    }

    /// The session's persona toggle (`None`: follow the settings), applied from the next request.
    pub fn set_persona(&self, persona: Option<bool>) {
        *sync::lock(&self.0.persona) = persona;
    }

    /// The root lane's model, applied from the next request.
    pub fn set_model(&self, id: &str) {
        *sync::lock(&self.0.model) = id.to_string();
    }

    /// The `alwaysSkills` problems of the prompt as last rendered.
    pub fn warnings(&self) -> Vec<String> {
        sync::lock(&self.0.current).2.clone()
    }
}
