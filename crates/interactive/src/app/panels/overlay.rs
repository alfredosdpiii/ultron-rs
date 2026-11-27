//! Full-frame overlays (search, copy/rewind pickers, model hub, agent hub) for [`super::super::App`].

use super::super::*;

impl App {
    /// The overlays' global keys: F5 retry, Alt+L display reset, Alt+A agent hub.
    pub(in crate::app) fn handle_overlay_keys(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "app.retry") {
            self.retry();
            return true;
        }
        if self.keys.matches(data, "app.display.reset") {
            self.reset_display = true;
            self.committed.clear();
            return true;
        }
        if self.keys.matches(data, "app.agents.hub") {
            self.send(LaneCommand::Agents);
            return true;
        }
        if self.keys.matches(data, "tui.altScreen.search") {
            let width = self.cols.max(1);
            let rows = self.document(width);
            let mut search = crate::transcript_search::TranscriptSearch::new(rows, self.keys.clone());
            let (t1, t2) = (Rc::clone(&self.theme), Rc::clone(&self.theme));
            search.set_styles(
                Box::new(move |s| format!("\x1b[4m{}\x1b[24m", t1.fg(Fg::Accent, s))),
                Box::new(move |s| format!("\x1b[7m{}\x1b[27m", t2.fg(Fg::Accent, s))),
            );
            self.overlay = Some(Overlay::Search(Box::new(search)));
            return true;
        }
        false
    }

    /// F5: send the failed turn's prompt again (the session keeps the failed attempt).
    pub(in crate::app) fn retry(&mut self) {
        if self.operation.is_some() || !crate::retry::failed_turn_tail(&self.transcript) {
            self.show_status("Nothing to retry");
            return;
        }
        let Some(prompt) = self
            .transcript
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| message_text(m).trim().to_string())
            .filter(|t| !t.is_empty())
        else {
            self.show_status("Nothing to retry");
            return;
        };
        self.status = "Retrying…".into();
        self.send(LaneCommand::Prompt {
            text: prompt,
            images: Vec::new(),
        });
    }

    /// Whether the display reset was asked for; clears the request.
    pub fn take_reset_display(&mut self) -> bool {
        std::mem::take(&mut self.reset_display)
    }

    pub(in crate::app) fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        self.transcript
            .iter()
            .enumerate()
            .map(|(i, m)| TranscriptEntry::new(&format!("m{i}"), "", m.clone()))
            .collect()
    }

    pub(in crate::app) fn picker(&self, mode: PickerMode, entries: Vec<TranscriptEntry>) -> TranscriptPicker {
        TranscriptPicker::new(
            entries,
            PickerOptions {
                mode,
                theme: Rc::clone(&self.theme),
                keys: self.keys.clone(),
                chat: self.chat.style(),
                title: None,
                action_label: None,
            },
        )
    }

    /// `/copy` with the overlays: pick a turn (or a block of it) over the transcript.
    pub(in crate::app) fn open_copy_picker(&mut self) {
        if self.last_assistant_text().is_none_or(|t| t.is_empty()) {
            self.show_status("Error: No agent messages to copy yet.");
            return;
        }
        let picker = self.picker(PickerMode::Copy, self.transcript_entries());
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    /// Esc-Esc: pick the user turn to continue from; the session forks before it (user messages carry their
    /// session entry ids, matched in order by text).
    pub(in crate::app) fn open_rewind(&mut self, forkable: Vec<crate::lane::ForkMessage>) {
        let mut ids = forkable.into_iter();
        let entries: Vec<TranscriptEntry> = self
            .transcript
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let id = if m["role"] == "user" {
                    let text = message_text(m);
                    ids.by_ref()
                        .find(|f| f.text == text)
                        .map_or(format!("m{i}"), |f| f.entry_id)
                } else {
                    format!("m{i}")
                };
                TranscriptEntry::new(&id, "", m.clone())
            })
            .collect();
        self.rewind_ids = entries
            .iter()
            .map(|e| (e.id.clone(), e.role() == "user" && !e.id.starts_with('m')))
            .collect();
        if !self.rewind_ids.iter().any(|(_, user)| *user) {
            self.show_status("No messages to rewind to");
            return;
        }
        let picker = self.picker(PickerMode::Rewind, entries);
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    pub(in crate::app) fn open_model_hub(&mut self) {
        let current = Some((
            format!("{}/{}", self.model.provider, self.model.id),
            self.thinking_level.clone(),
        ));
        let hub = ModelHub::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            self.models.clone(),
            self.recent_models.clone(),
            current,
        );
        self.overlay = Some(Overlay::ModelHub(Box::new(hub)));
    }

    pub(in crate::app) fn handle_overlay_input(&mut self, data: &str) {
        self.overlay_input(data);
        if self.overlay.is_none() {
            self.reset_display = true;
            self.committed.clear();
        }
    }

    pub(in crate::app) fn overlay_input(&mut self, data: &str) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        match overlay {
            Overlay::Search(search) => {
                if matches!(
                    search.handle_input(data),
                    crate::transcript_search::SearchOutcome::Close
                ) {
                    self.overlay = None;
                }
            }
            Overlay::Picker(picker) => {
                let event = picker.handle_input(data);
                self.handle_picker_event(event);
            }
            Overlay::ModelHub(hub) => {
                let event = hub.handle_input(data);
                self.handle_model_hub_event(event);
            }
            Overlay::AgentHub(_) => self.handle_agent_hub_input(data),
        }
    }

    fn handle_picker_event(&mut self, event: PickerEvent) {
        match event {
            PickerEvent::Copy { content, label, .. } => {
                self.overlay = None;
                self.copy_to_clipboard(&content, &format!("Copied {label} to clipboard"));
            }
            PickerEvent::Open { href, .. } => {
                self.overlay = None;
                match self.viewport.open_url.clone() {
                    Some(open) => {
                        open(&href);
                        self.show_status(&format!("Opened {href}"));
                    }
                    None => self.show_status("Error: no browser opener"),
                }
            }
            PickerEvent::Rewind { entry_id } => self.handle_picker_rewind(&entry_id),
            PickerEvent::RestoreFiles { entry_id } => {
                self.overlay = None;
                match self.rewind_target(&entry_id) {
                    Some(entry) => self.ask_restore(entry, true),
                    None => self.show_status("Files are already at this point"),
                }
            }
            PickerEvent::Cancel => self.overlay = None,
            PickerEvent::Ignored | PickerEvent::Render => {}
        }
    }

    fn handle_picker_rewind(&mut self, entry_id: &str) {
        self.overlay = None;
        let Some(entry_id) = self.rewind_target(entry_id) else {
            self.show_status("Already at this point");
            return;
        };
        if self.operation.is_some() {
            self.send(LaneCommand::Abort);
        }
        self.send(LaneCommand::Fork(entry_id));
    }

    fn handle_model_hub_event(&mut self, event: ModelHubEvent) {
        match event {
            ModelHubEvent::Use { selector, thinking } => {
                self.overlay = None;
                let Some(model) = self
                    .models
                    .iter()
                    .find(|m| format!("{}/{}", m.provider, m.id) == selector)
                    .cloned()
                else {
                    self.show_status(&format!("Error: Unknown model: {selector}"));
                    return;
                };
                self.recent_models.retain(|s| *s != selector);
                self.recent_models.insert(0, selector);
                self.select_model(model);
                if let Some(level) = thinking {
                    self.set_thinking(&level);
                }
            }
            ModelHubEvent::Cancel => self.overlay = None,
            ModelHubEvent::Ignored | ModelHubEvent::Render => {}
        }
    }

    fn handle_agent_hub_input(&mut self, data: &str) {
        let Some(Overlay::AgentHub(hub)) = self.overlay.as_mut() else {
            return;
        };
        match hub.handle_input(data) {
            HubEvent::Close => self.overlay = None,
            HubEvent::Kill(id) => {
                self.send(LaneCommand::KillAgent(id));
                self.send(LaneCommand::Agents);
            }
            HubEvent::OpenTranscript(id) => {
                hub.set_transcript(&id, Vec::new());
                self.send(LaneCommand::AgentTranscript(id));
            }
            HubEvent::Revive(id) => {
                self.send(LaneCommand::ReviveAgent(id));
                self.send(LaneCommand::Agents);
            }
            HubEvent::Steer { id, text } => {
                let parked = hub
                    .rows()
                    .iter()
                    .any(|r| r.id == id && r.status == crate::agent_hub::AgentStatus::Parked);
                if parked {
                    self.send(LaneCommand::ReviveAgent(id));
                    self.send(LaneCommand::Agents);
                } else {
                    self.send(LaneCommand::SteerAgent {
                        id: id.to_string(),
                        text,
                    });
                    self.send(LaneCommand::AgentTranscript(id));
                }
            }
            HubEvent::Ignored | HubEvent::Render => {}
        }
    }

    /// The open overlay's frame, when one is open.
    pub(in crate::app) fn overlay_frame(&mut self, width: usize, height: usize) -> Option<Vec<String>> {
        let now = jsrt::date::now_ms();
        Some(match self.overlay.as_mut()? {
            Overlay::Search(search) => search.render(width, height),
            Overlay::Picker(picker) => picker.render(width, height),
            Overlay::ModelHub(hub) => hub.render(width, height),
            Overlay::AgentHub(hub) => hub.render(width, height, now),
        })
    }
}
