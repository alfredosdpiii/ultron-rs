use std::sync::{Arc, Mutex};

use ai::transcript::create_initial_system_message;
use ai::{
    AbortSignal, AssistantContent, ImageContent, Message, Model, StopReason, TextContent, UserContent, UserMessage,
    UserMessageContent,
};

use crate::agent_loop::agent_loop;
use crate::types::{AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, BeforeToolCall, StreamFn};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

struct Queue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl Queue {
    fn peek(&self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.clone(),
            QueueMode::OneAtATime => self.messages.first().cloned().into_iter().collect(),
        }
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        let drained = self.peek();
        self.messages.drain(..drained.len());
        drained
    }
}

type Shared<T> = Arc<Mutex<T>>;

/// Cloneable handle for listeners and other tasks: queue steering messages and abort the active run.
#[derive(Clone)]
pub struct AgentControl {
    steering: Shared<Queue>,
    active: Shared<Option<AbortSignal>>,
}

impl AgentControl {
    /// Queue a message delivered after the current turn's tool calls.
    pub fn steer(&self, message: AgentMessage) {
        jsrt::sync::lock(&self.steering).messages.push(message);
    }

    pub fn abort(&self) {
        if let Some(signal) = jsrt::sync::lock(&self.active).as_ref() {
            signal.abort();
        }
    }
}

pub struct AgentOptions {
    pub model: Model,
    pub system_prompt: Option<String>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    /// `"off"` or a reasoning level.
    pub thinking_level: String,
    pub steering_mode: QueueMode,
    pub session_id: Option<String>,
    pub before_tool_call: Option<BeforeToolCall>,
}

impl AgentOptions {
    pub fn new(model: Model) -> Self {
        AgentOptions {
            model,
            system_prompt: None,
            tools: Vec::new(),
            messages: Vec::new(),
            thinking_level: "off".into(),
            steering_mode: QueueMode::OneAtATime,
            session_id: None,
            before_tool_call: None,
        }
    }
}

type Listener = Box<dyn FnMut(&AgentEvent) + Send>;

/// `Agent`: owns the transcript and tools, runs prompts through the loop, and keeps state from the events.
pub struct Agent {
    pub model: Model,
    pub thinking_level: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub messages: Vec<AgentMessage>,
    pub session_id: Option<String>,
    pub error_message: Option<String>,
    pub before_tool_call: Option<BeforeToolCall>,
    stream_fn: Box<dyn StreamFn>,
    listeners: Vec<Listener>,
    control: AgentControl,
}

impl Agent {
    pub fn new(options: AgentOptions, stream_fn: Box<dyn StreamFn>) -> Self {
        let mut messages = options.messages;
        let declarations: Vec<ai::Tool> = options.tools.iter().map(|t| t.tool().clone()).collect();
        if !matches!(messages.first(), Some(Message::System(_)))
            && let Some(initial) = create_initial_system_message(options.system_prompt.as_deref(), Some(&declarations))
        {
            messages.insert(0, Message::System(initial));
        }
        let queue = |mode| {
            Arc::new(Mutex::new(Queue {
                messages: Vec::new(),
                mode,
            }))
        };
        Agent {
            model: options.model,
            thinking_level: options.thinking_level,
            tools: options.tools,
            messages,
            session_id: options.session_id,
            error_message: None,
            before_tool_call: options.before_tool_call,
            stream_fn,
            listeners: Vec::new(),
            control: AgentControl {
                steering: queue(options.steering_mode),
                active: Arc::new(Mutex::new(None)),
            },
        }
    }

    pub fn control(&self) -> AgentControl {
        self.control.clone()
    }

    pub fn subscribe(&mut self, listener: impl FnMut(&AgentEvent) + Send + 'static) {
        self.listeners.push(Box::new(listener));
    }

    pub async fn prompt_text(&mut self, text: &str, images: Vec<ImageContent>) {
        let mut content = vec![UserContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })];
        content.extend(images.into_iter().map(UserContent::Image));
        let message = Message::User(UserMessage {
            content: UserMessageContent::Blocks(content),
            timestamp: jsrt::date::now_ms(),
        });
        self.prompt(vec![message]).await;
    }

    /// The run's reply: the text of the transcript's last assistant message; `Err` when that message failed or was
    /// aborted (its error), or there is none.
    pub fn final_text(&self) -> Result<String, String> {
        let last = self.messages.iter().rev().find_map(|m| match m {
            Message::Assistant(message) => Some(message),
            _ => None,
        });
        let Some(last) = last else {
            return Err("Agent produced no assistant result at the completed tip".into());
        };
        if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
            return Err(last.error_message.clone().unwrap_or_else(|| "Agent run failed".into()));
        }
        Ok(last
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    /// Run the loop over `messages`; returns when the run ends (`agent_end`).
    pub async fn prompt(&mut self, messages: Vec<AgentMessage>) {
        let signal = AbortSignal::new();
        *jsrt::sync::lock(&self.control.active) = Some(signal.clone());
        self.error_message = None;

        let steering = self.control.steering.clone();
        let config = AgentLoopConfig {
            model: self.model.clone(),
            reasoning: (self.thinking_level != "off").then(|| self.thinking_level.clone()),
            session_id: self.session_id.clone(),
            api_key: None,
            get_steering_messages: Some(Box::new(move || jsrt::sync::lock(&steering).drain())),
            before_tool_call: self.before_tool_call.clone(),
        };
        let context = AgentContext {
            messages: self.messages.clone(),
            tools: self.tools.clone(),
        };
        let Agent {
            messages: state,
            error_message,
            listeners,
            stream_fn,
            ..
        } = self;
        let mut process = |event: AgentEvent| {
            match &event {
                AgentEvent::MessageEnd { message } => state.push(message.clone()),
                AgentEvent::TurnEnd {
                    message: Message::Assistant(message),
                    ..
                } if message.error_message.is_some() => {
                    error_message.clone_from(&message.error_message);
                }
                _ => {}
            }
            for listener in listeners.iter_mut() {
                listener(&event);
            }
        };
        agent_loop(messages, context, config, &mut process, &signal, stream_fn.as_ref()).await;

        *jsrt::sync::lock(&self.control.active) = None;
    }
}
