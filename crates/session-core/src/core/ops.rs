//! Operation dispatch and synchronous session queries/mutations for [`super::Core`].

use super::*;

impl Core {
    pub(super) fn op(&mut self, op: Op, reply: Reply) {
        match op {
            Op::Prompt { message, images, queue } => {
                let (message, notice) = self.user_text(&message);
                let result = if self.run.is_some() {
                    match queue {
                        Some(kind) => self.enqueue(kind, message, images, None).map(drop),
                        None => Err("Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.".into()),
                    }
                } else {
                    self.prompt(message, images, None).map_err(|(error, _)| error)
                };
                reply(result.map(|()| notice.map(|notice| json!({"keepGoing": notice}))));
            }
            Op::PromptAndWait { message, images } => {
                let (message, _) = self.user_text(&message);
                if self.run.is_some() {
                    return reply(Err("Agent is already processing".into()));
                }
                if let Err((error, reply)) = self.prompt(message, images, Some(reply)) {
                    reply(Err(error));
                }
            }
            bash @ Op::Bash { .. } => self.bash(bash, reply),
            Op::AppendCustom { custom_type, data } => self.append_custom(custom_type, data, reply),
            Op::BugSummary { hint } => {
                let summarizer = self.summarizer();
                let messages = view::messages(&self.pi());
                let retry = self.summary_retry();
                let signal = AbortSignal::new();
                static SUMMARIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let number = SUMMARIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.bug_summary = Some((number, signal.clone()));
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    reply(
                        summarizer
                            .bug_report(&messages, hint.as_deref(), retry, &signal)
                            .map(|text| Some(json!(text))),
                    );
                    let _ = tx.send(Input::BugSummaryEnded(number));
                });
            }
            compact @ Op::Compact { .. } if self.run.is_some() => {
                self.abort();
                self.after_run.push(Input::Op(compact, reply));
            }
            Op::Recover => {
                let result = self.recover();
                reply(result);
            }
            other => {
                let result = self.sync_op(other);
                reply(result);
            }
        }
    }

    /// A prompt's or queued message's text as the session takes it: `/review` expanded (agent-controller-provider.ts
    /// `toTextPrompt`, crate::review_command), seen by the persona as that text (`persona_user_input`), then a prompt
    /// template's `/name args` expanded. The persona's keep-going notice, if any.
    fn user_text(&mut self, message: &str) -> (String, Option<String>) {
        let message = crate::review_command::expand_review_command(message);
        let notice = self.persona_user_input(&message);
        self.turn_guard_user_input();
        (self.expand_prompt_template(message), notice)
    }

    /// AgentSession `expandPromptTemplate`: a prompt or queued message `/name args` naming one of the session's
    /// prompt templates (the `/name` commands it lists) becomes the template's text with the arguments substituted.
    fn expand_prompt_template(&self, message: String) -> String {
        if !message.starts_with('/') {
            return message;
        }
        resources::prompt_templates::expand_prompt_template(&message, &self.lane.stack.prompt_templates())
    }

    fn sync_op(&mut self, op: Op) -> OpResult {
        if matches!(op, Op::Describe) {
            self.lane.stack.stats.flush();
        }
        match op {
            Op::Prompt { .. }
            | Op::PromptAndWait { .. }
            | Op::Bash { .. }
            | Op::BugSummary { .. }
            | Op::AppendCustom { .. }
            | Op::Recover => {
                unreachable!("answered in op")
            }
            Op::Enqueue {
                kind,
                message,
                images,
                entry_id,
            } => {
                let (message, _) = self.user_text(&message);
                self.enqueue(kind, message, images, entry_id).map(|id| Some(json!(id)))
            }
            Op::CancelQueued { entry_id } => {
                let found = sync::lock(&self.queues).items.iter().any(|q| q.entry_id == entry_id);
                if found {
                    self.remove_queued(&[entry_id])?;
                }
                Ok(Some(json!(found)))
            }
            Op::ClearQueue => Ok(Some(self.clear_queue())),
            Op::Abort => {
                self.abort();
                Ok(None)
            }
            Op::State => Ok(Some(self.state())),
            Op::AvailableModels => {
                self.models = lane::available_models(&self.env);
                Ok(Some(Value::Array(
                    self.models
                        .iter()
                        .filter_map(|m| serde_json::to_value(m).ok())
                        .collect(),
                )))
            }
            Op::SetModel { provider, model_id } => {
                let find = |models: &[Model]| {
                    models
                        .iter()
                        .find(|m| m.provider == provider && m.id == model_id)
                        .cloned()
                };
                let model = match find(&self.models) {
                    Some(model) => model,
                    None => {
                        self.models = lane::available_models(&self.env);
                        find(&self.models).ok_or_else(|| format!("Model not found: {provider}/{model_id}"))?
                    }
                };
                self.model = model;
                self.model_unavailable = false;
                self.lane.stack.set_root_model(&self.model);
                self.store_lane_config()?;
                serde_json::to_value(&self.model).map(Some).map_err(|e| e.to_string())
            }
            Op::CycleModel => {
                if self.models.len() < 2 {
                    return Ok(Some(Value::Null));
                }
                let index = self
                    .models
                    .iter()
                    .position(|m| m.provider == self.model.provider && m.id == self.model.id);
                self.model = self.models[index.map_or(0, |i| (i + 1) % self.models.len())].clone();
                self.model_unavailable = false;
                self.lane.stack.set_root_model(&self.model);
                self.store_lane_config()?;
                Ok(Some(json!({
                    "model": serde_json::to_value(&self.model).map_err(|e| e.to_string())?,
                    "thinkingLevel": self.thinking,
                    "isScoped": false,
                })))
            }
            Op::ThinkingLevels => Ok(Some(json!(self.thinking_levels()))),
            Op::SetThinking { level } => self.set_thinking(&level).map(|()| None),
            Op::CycleThinking => {
                let levels = self.thinking_levels();
                if levels.len() < 2 {
                    return Ok(Some(Value::Null));
                }
                let index = levels.iter().position(|l| *l == self.thinking);
                let next = levels[index.map_or(0, |i| (i + 1) % levels.len())].clone();
                self.set_thinking(&next)?;
                Ok(Some(json!(next)))
            }
            Op::SetQueueMode { kind, mode } => {
                let mut queues = sync::lock(&self.queues);
                let all = mode == config::keys::QUEUE_ALL;
                match kind {
                    QueueKind::Steer => queues.steering_all = all,
                    QueueKind::FollowUp => queues.follow_up_all = all,
                    QueueKind::NextRun => return Err("kind must be steer or followUp".into()),
                }
                Ok(None)
            }
            Op::Compact { instructions } => self.compact(instructions.as_deref()).map(Some),
            Op::SetAutoCompaction(enabled) => {
                self.auto_compaction = enabled;
                Ok(None)
            }
            Op::SetAutoRetry(enabled) => {
                self.auto_retry = enabled;
                self.durable
                    .auto_retry
                    .store(enabled, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            }
            Op::SetTerminalTheme { theme } => {
                if theme != "light" && theme != "dark" {
                    return Err(format!("theme must be light or dark, not {theme:?}"));
                }
                *self
                    .runtime
                    .config
                    .terminal_theme
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(theme);
                Ok(None)
            }
            Op::AbortRetry => {
                if self
                    .run
                    .as_ref()
                    .and_then(|run| run.journal.run())
                    .is_some_and(|journal| journal.retry_waiting().is_some())
                {
                    self.abort();
                }
                Ok(None)
            }
            Op::AbortBash => {
                if let Some(signal) = &self.bash {
                    signal.abort("abort_bash");
                }
                Ok(None)
            }
            Op::AbortBugSummary => {
                if let Some((_, signal)) = self.bug_summary.take() {
                    signal.abort();
                }
                Ok(None)
            }
            Op::ShareContext => {
                let tool = agent::AgentTool::tool(&*self.lane.tool);
                Ok(Some(json!({
                    "systemPrompt": self.runtime.system.current(),
                    "tools": [{"name": tool.name, "description": tool.description, "parameters": tool.parameters}],
                })))
            }
            Op::Stats => Ok(Some(view::stats(
                &self.pi(),
                Some(&self.session_file()),
                lock_session(&self.session).id(),
                self.model.context_window,
            ))),
            Op::ForkMessages => Ok(Some(Value::Array(view::fork_messages(&self.pi())))),
            Op::Entries { since } => {
                let pi = self.pi();
                let mut entries = view::entries(&pi);
                if let Some(since) = since {
                    let index = entries
                        .iter()
                        .position(|e| e["id"] == since.as_str())
                        .ok_or_else(|| format!("Entry not found: {since}"))?;
                    entries.drain(..=index);
                }
                Ok(Some(
                    json!({"entries": entries, "leafId": pi.leaf_id().cloned().unwrap_or(Value::Null)}),
                ))
            }
            Op::Tree => Ok(Some(self.labeled_tree())),
            Op::LastAssistantText => Ok(Some(json!(view::last_assistant_text(&self.pi())))),
            Op::SetName { name } => {
                names::set_user_name(&mut lock_session(&self.session), Some(&name))?;
                self.name_changed();
                self.session_info_changed();
                Ok(None)
            }
            Op::Messages => Ok(Some(Value::Array(view::messages(&self.pi())))),
            Op::LaneSnapshot => Ok(Some(self.snapshot())),
            Op::ForkSource { entry_id } => {
                self.idle()?;
                let pi = self.pi();
                let entry = pi
                    .entry(&entry_id)
                    .filter(|e| e["type"] == "message" && e["message"]["role"] == "user")
                    .ok_or_else(|| format!("Entry is not a user message: {entry_id}"))?;
                Ok(Some(json!(message_text(&entry["message"]))))
            }
            Op::CloneSource => {
                self.idle()?;
                let leaf = self.pi().leaf_id().cloned().filter(|l| !l.is_null());
                leaf.map(Some)
                    .ok_or_else(|| "Cannot clone session: no current entry selected".to_string())
            }
            Op::Forked { target } => {
                let metadata = session_journal::catalog::read_metadata(Path::new(&target))
                    .ok_or_else(|| format!("Not a session file: {target}"))?;
                let mut session = self.repo.open(&metadata)?;
                session.commit(&[Write::set(
                    SESSION_START.0,
                    SESSION_START.1,
                    json!({"previousSessionFile": self.session_file()}),
                )])?;
                Ok(None)
            }
            Op::Describe => Ok(Some(json!({
                "sessionId": lock_session(&self.session).id(),
                "sessionFile": self.session_file(),
                "name": lock_session(&self.session).name(),
                "messages": view::messages(&self.pi()),
                "entries": lock_session(&self.session).branch_entries(MAIN_LANE).unwrap_or_default(),
                "model": serde_json::to_value(&self.model).unwrap_or(Value::Null),
                "thinkingLevel": self.thinking,
            }))),
            Op::Navigate {
                target_id,
                summarize,
                instructions,
                label,
            } => self
                .navigate(target_id, summarize, instructions.as_deref(), label)
                .map(Some),
            Op::SetLabel { entry_id, label } => self.set_label(&entry_id, label.as_deref()).map(|()| None),
            Op::Checkpoints => Ok(Some(self.list_checkpoints())),
            Op::ScheduleFire { id, due } => self.schedule_fire(&id, due),
            Op::ScheduleCancel { id } => self.schedule_cancel(&id),
            Op::Persona { args } => self.persona_command("persona", &args).map(Some),
            Op::KeepGoing { args } => self.persona_command("keepgoing", &args).map(Some),
            Op::ContinueFrom { session } => self.continue_from(&session).map(Some),
            Op::RestoreFiles { entry_id, checkpoint } => self.restore_files(entry_id, checkpoint).map(Some),
            Op::Agents => self.lane.stack.delegation.subagent_views(MAIN_LANE).map(Some),
            Op::KillAgent { id } => self
                .lane
                .stack
                .delegation
                .cancel(&id, "Killed from the agent hub")
                .map(Some),
            Op::SteerAgent { id, message } => self.steer_agent(&id, &message).map(Some),
            Op::ReviveAgent { id } => self.revive_agent(&id).map(Some),
            Op::AgentTranscript { id } => self.agent_transcript(&id).map(Some),
            Op::StartConversation { message } => self.start_conversation(&message).map(Some),
            Op::ConversationInput { id, message } => self.conversation_input(id, &message).map(|()| None),
            Op::Conversations { id } => self.conversations(id).map(Some),
            Op::AsyncPending => Ok(Some(self.async_pending())),
            Op::Commands => Ok(Some(Value::Array(self.lane.stack.commands()))),
            Op::Inspect { request, .. } if request == "async.pending" => Ok(Some(self.async_pending())),
            Op::Inspect { request, .. } if request == "rlm.detached" => Ok(Some(
                json!({ "cells": self.lane.stack.lanes.detached_cells(MAIN_LANE) }),
            )),
            Op::Inspect { request, .. } if request == "rlm.pool" => Ok(Some(self.lane.stack.pool_stats())),
            Op::Inspect { request, payload } => {
                let runtime = self.lane.stack.cell_handler(MAIN_LANE, None);
                crate::inspect::inspect(Some(&*runtime), &request, payload, &kernel::Signal::new()).map(Some)
            }
            Op::Goal { args } => self.lane.stack.goal.command(&args).map(|text| Some(json!(text))),
            Op::Observer { args } => self.observer_command(&args).map(|text| Some(json!(text))),
            Op::ListSessions => {
                let listed =
                    names::list_sessions(Path::new(self.repo.root()), &lock_session(&self.session).metadata().cwd);
                Ok(Some(Value::Array(
                    listed
                        .into_iter()
                        .map(|s| {
                            json!({"path": s.metadata.path.display().to_string(), "id": s.metadata.id, "name": s.name,
                                "modified": s.metadata.modified_at as i64, "messageCount": s.message_count,
                                "firstMessage": s.first_message, "allMessagesText": s.all_messages_text,
                                "cwd": s.metadata.cwd})
                        })
                        .collect(),
                )))
            }
        }
    }

    fn thinking_levels(&self) -> Vec<String> {
        ai::models::supported_thinking_levels(&self.model)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn set_thinking(&mut self, level: &str) -> Result<(), String> {
        let levels = self.thinking_levels();
        if !levels.iter().any(|l| l == level) {
            return Err(format!(
                "Thinking level {level} is unavailable; choose one of: {}",
                levels.join(", ")
            ));
        }
        if self.thinking != level {
            self.thinking = level.to_string();
            self.store_lane_config()?;
            self.emit(WatchEvent::ConfigUpdate {
                lane: MAIN_LANE.into(),
                property: "thinkingLevel".into(),
                value: json!(level),
            });
        }
        Ok(())
    }

    pub(super) fn state(&self) -> Value {
        let queues = sync::lock(&self.queues);
        let mut state = Map::new();
        state.insert("model".into(), serde_json::to_value(&self.model).unwrap_or(Value::Null));
        state.insert("thinkingLevel".into(), self.thinking.clone().into());
        state.insert("isStreaming".into(), self.run.is_some().into());
        state.insert("isCompacting".into(), self.compacting.is_some().into());
        if self.turn_guard_checking() {
            state.insert("isChecking".into(), true.into());
        }
        let mode = |all: bool| {
            if all {
                config::keys::QUEUE_ALL
            } else {
                config::keys::QUEUE_ONE_AT_A_TIME
            }
        };
        state.insert("steeringMode".into(), mode(queues.steering_all).into());
        state.insert("followUpMode".into(), mode(queues.follow_up_all).into());
        state.insert("sessionFile".into(), self.session_file().into());
        state.insert("sessionId".into(), lock_session(&self.session).id().into());
        if let Some(name) = lock_session(&self.session).name() {
            state.insert("sessionName".into(), name.into());
        }
        state.insert("autoCompactionEnabled".into(), self.auto_compaction.into());
        state.insert("messageCount".into(), view::messages(&self.pi()).len().into());
        state.insert(
            "pendingMessageCount".into(),
            (queues.items.len() + queues.delivered.len()).into(),
        );
        Value::Object(state)
    }
}
