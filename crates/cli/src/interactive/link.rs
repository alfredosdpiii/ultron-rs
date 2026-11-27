//! The link thread: owns the session [`Link`] and runs the TUI's commands one at a time.

use super::support::{apply_presentation_plugins, credentials_saved, image_values, model_info};
use super::{WORKER_RECOVERY_INTERVAL, opened};
use crate::Env;
use client::session::SessionLink as Link;
use interactive::lane::{LaneCommand, LaneEvent, PromptImage};
use interactive::run::UiEvent;
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::{Arc, mpsc};

/// What the link thread runs: the TUI's commands, and the client's own.
pub(super) enum Command {
    Lane(LaneCommand),
    /// The link saw the session's worker restart under it (its `connection` event `reconnecting`).
    WorkerStopped,
    /// The run ended: report the session's name (a model title may have replaced the automatic one).
    RefreshName,
    /// The session was re-attached after its Radius connection recovered: show it again (`#openLane`).
    Reopen,
    /// A subscription login saved its credential (provider, name): report it as `runLoginDialog` does.
    LoginSaved(String, String),
    Shutdown,
}

/// The flags the link thread's long commands watch, set by [`super::route`] without waiting on the link.
#[derive(Clone, Default)]
pub(super) struct Cancels {
    /// Aborted when the user cancels `/share`'s gist upload; a fresh one for each `/share`.
    pub(super) share: Arc<std::sync::Mutex<kernel::Signal>>,
    /// Set when the user cancels `/bug`'s upload.
    pub(super) bug: Arc<std::sync::atomic::AtomicBool>,
}

pub(super) struct LinkSession {
    pub(super) link: Link,
    pub(super) ui: mpsc::Sender<UiEvent>,
    pub(super) catalog_env: Env,
    pub(super) reporter: crate::bug_report::Reporter,
    pub(super) herdr: Option<Arc<crate::herdr_report::TuiReport>>,
    pub(super) cancels: Cancels,
    pub(super) name: Option<String>,
    pub(super) last_recovery: Option<std::time::Instant>,
    pub(super) terminal_theme: Option<String>,
}

impl LinkSession {
    pub(super) fn send(&self, event: LaneEvent) {
        let _ = self.ui.send(UiEvent::Lane(event));
    }

    fn report_name(&mut self) {
        let current = self
            .link
            .op(&Op::Describe)
            .ok()
            .flatten()
            .and_then(|d| d["name"].as_str().map(str::to_string));
        if current != self.name {
            self.name = current.clone();
            if let Some(herdr) = &self.herdr {
                herdr.name(current.clone());
            }
            self.send(LaneEvent::Name(current));
        }
    }

    pub(super) fn open(&mut self) {
        match opened(&self.link) {
            Ok((session, model, entries, thinking)) => {
                self.name = session.name.clone();
                let prepared = self.link.connection().prepare_session(&session.session_id);
                if let Ok(data) = prepared
                    && let Err(error) = apply_presentation_plugins(&data.unwrap_or(Value::Null))
                {
                    self.send(LaneEvent::Status(format!("Error: {error}")));
                }
                if let Some(herdr) = &self.herdr {
                    herdr.opened(&session, &model);
                }
                self.send(LaneEvent::Opened(session));
                self.send(LaneEvent::Entries(entries));
                self.send(LaneEvent::Model(model));
                if let Some(level) = thinking {
                    self.send(LaneEvent::Thinking(level));
                }
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn run_op(&self, op: Op) {
        if let Err(error) = self.link.op(&op) {
            self.send(LaneEvent::Error(error));
        }
    }

    fn persona(&self, op: Op) {
        self.send(LaneEvent::Status(match self.link.op(&op) {
            Ok(reply) => reply
                .as_ref()
                .and_then(|r| r["text"].as_str())
                .unwrap_or("No persona reply")
                .to_string(),
            Err(error) => format!("Error: {error}"),
        }));
    }

    fn handle_worker_stopped(&mut self) {
        if self
            .link
            .remote_route()
            .is_some_and(|route| matches!(route, client::session::RemoteRoute::Radius { .. }))
        {
            return;
        }
        let now = std::time::Instant::now();
        if self
            .last_recovery
            .is_some_and(|t| now.duration_since(t) < WORKER_RECOVERY_INTERVAL)
        {
            self.send(LaneEvent::Status(format!(
                "Error: The session worker stopped again; {}",
                client::lost::RESUME_HINT
            )));
            return;
        }
        self.last_recovery = Some(now);
        self.send(LaneEvent::Status(
            "The session worker stopped unexpectedly; reattaching the Session…".into(),
        ));
        let id = self.link.session_id();
        match self.link.attach(&id) {
            Ok(()) => {
                self.open();
                self.send(LaneEvent::Status(
                    "The session worker stopped unexpectedly and was restarted; the Session is intact, but work running in the old worker (such as background jobs) was lost.".into(),
                ));
            }
            Err(error) => self.send(LaneEvent::Status(format!("Error: {error}"))),
        }
    }

    pub(super) fn handle_new_or_continue(&mut self, continued: Option<String>) {
        let previous = self.link.session_id();
        let created = self
            .link
            .create(json!({}))
            .and_then(|id| self.link.attach(&id).map(|()| id));
        let created = match (created, &continued) {
            (Ok(id), Some(old)) => self.link.op(&Op::ContinueFrom { session: old.clone() }).map(|_| id),
            (created, _) => created,
        };
        match created {
            Ok(id) => {
                if let Err(error) = crate::durable_client::follow_home(&previous, &id) {
                    self.send(LaneEvent::Status(format!(
                        "Error: the home session was not moved: {error}"
                    )));
                }
                if let Some(herdr) = &self.herdr {
                    herdr.new_session();
                }
                self.open();
                self.send(LaneEvent::Status(match &continued {
                    Some(old) => format!("✓ New session started; it searches session {old}"),
                    None => "✓ New session started".into(),
                }));
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn handle_prompt(&mut self, text: String, images: Vec<PromptImage>) {
        match self.link.op(&Op::Prompt {
            message: text,
            images: image_values(images),
            queue: None,
        }) {
            Ok(reply) => {
                if let Some(notice) = reply.as_ref().and_then(|r| r["keepGoing"].as_str()) {
                    self.send(LaneEvent::Status(notice.to_string()));
                }
                self.report_name();
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn handle_select_model(&self, provider: String, model_id: String) {
        match self.link.op(&Op::SetModel { provider, model_id }) {
            Ok(model) => {
                let model = model_info(&model.unwrap_or_default());
                if let Some(herdr) = &self.herdr {
                    herdr.model(&model);
                }
                self.send(LaneEvent::Model(model));
            }
            Err(error) => self.send(LaneEvent::Error(error)),
        }
    }

    fn handle_prompt_lane(&mut self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::Enqueue {
                kind,
                text,
                images,
                entry_id,
            } => self.run_op(Op::Enqueue {
                kind,
                message: text,
                images: image_values(images),
                entry_id: Some(entry_id),
            }),
            LaneCommand::CancelQueued(entry_id) => self.run_op(Op::CancelQueued { entry_id }),
            LaneCommand::SetName(name) => self.run_op(Op::SetName { name }),
            LaneCommand::Prompt { text, images } => self.handle_prompt(text, images),
            LaneCommand::SelectModel { provider, id } => self.handle_select_model(provider, id),
            LaneCommand::SetThinking(level) => self.run_op(Op::SetThinking { level }),
            other => return Some(other),
        }
        None
    }

    fn handle_persona_and_ui_lane(&self, command: LaneCommand) -> Option<LaneCommand> {
        match command {
            LaneCommand::ExtensionUiRespond { id, answer } => {
                let connection = self.link.connection();
                let _ = connection.session_target().and_then(|target| {
                    connection.call(target, protocol::names::EXTENSION_UI, "respond", json!([id, answer]))
                });
            }
            LaneCommand::Persona(args) => self.persona(Op::Persona { args }),
            LaneCommand::KeepGoing(args) => self.persona(Op::KeepGoing { args }),
            LaneCommand::ReadPersona => self.persona(Op::Persona { args: String::new() }),
            other => return Some(other),
        }
        None
    }

    fn handle_lane(&mut self, command: LaneCommand) {
        let Some(cmd) = self.handle_prompt_lane(command) else {
            return;
        };
        let Some(cmd) = self.handle_persona_and_ui_lane(cmd) else {
            return;
        };
        let Some(cmd) = self.handle_session_lifecycle(cmd) else {
            return;
        };
        let Some(cmd) = self.handle_tree_and_agents(cmd) else {
            return;
        };
        let Some(cmd) = self.handle_auth_and_settings(cmd) else {
            return;
        };
        self.handle_tools_and_reports(cmd);
    }
}

/// The link thread: owns the link, runs the TUI's commands one at a time.
#[expect(
    clippy::too_many_arguments,
    reason = "internal wiring helper; parameters match distinct caller inputs"
)]
pub(super) fn link_thread(
    link: Link,
    commands: mpsc::Receiver<Command>,
    ui: mpsc::Sender<UiEvent>,
    no_session: bool,
    signalled: Arc<std::sync::atomic::AtomicI32>,
    catalog_env: Env,
    reporter: crate::bug_report::Reporter,
    herdr: Option<Arc<crate::herdr_report::TuiReport>>,
    cancels: Cancels,
) {
    let mut session = LinkSession {
        link,
        ui,
        catalog_env,
        reporter,
        herdr,
        cancels,
        name: None,
        last_recovery: None,
        terminal_theme: None,
    };
    session.open();
    let mut extension_ui = crate::link::ExtensionUiPoll::default();
    loop {
        for request in extension_ui.poll(&session.link.session_id(), &session.link.connection()) {
            session.send(LaneEvent::ExtensionUi(request));
        }
        let command = match commands.recv_timeout(std::time::Duration::from_millis(250)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        };
        match command {
            Command::WorkerStopped => session.handle_worker_stopped(),
            Command::RefreshName => session.report_name(),
            Command::Reopen => session.open(),
            Command::LoginSaved(provider, name) => {
                let _ = session_core::model_refresh::refresh_catalogs(&session.catalog_env);
                let send_fn = |event: LaneEvent| {
                    let _ = session.ui.send(UiEvent::Lane(event));
                };
                credentials_saved(
                    &session.link,
                    &session.catalog_env,
                    &send_fn,
                    "Logged in to",
                    &provider,
                    &name,
                );
            }
            Command::Lane(cmd) => session.handle_lane(cmd),
            Command::Shutdown => break,
        }
    }
    if signalled.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        let _ = session.link.op(&Op::Abort);
        session.link.close(no_session);
    } else {
        session.link.close(false);
    }
}
