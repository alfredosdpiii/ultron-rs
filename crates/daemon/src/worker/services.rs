//! The session services exposed by a session worker over Chord.

use super::{Extensions, LifecycleRef, RESTART_EXIT_CODE, Watch, call_core, queue, request, save_default_model};
use crate::chord::{Provider, Service, ServiceError, ServiceResult};
use crate::lane::Lane;
use protocol::watch::QueueKind;
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::core::{Input, Op};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

fn run_prompt_method(core: &Sender<Input>, lifecycle: &LifecycleRef, args: &[Value]) -> ServiceResult {
    let typed = request(args)["message"]
        .as_str()
        .ok_or_else(|| ServiceError::internal("Invalid prompt request"))?;
    if let Some((name, op)) = crate::session_commands::command(typed) {
        return Ok(Some(crate::session_commands::answer(name, call_core(core, op)?)));
    }
    let message = typed.to_string();
    let images = request(args)["images"].as_array().cloned().unwrap_or_default();
    let lifecycle = jsrt::sync::lock(lifecycle).clone();
    let marker = format!("prompt-{}", jsrt::random::uuid_v4());
    if let Some(lifecycle) = &lifecycle {
        lifecycle.operation_started("run", "main", &marker);
    }
    let result = call_core(core, Op::PromptAndWait { message, images });
    if let Some(lifecycle) = &lifecycle {
        lifecycle.operation_stopped("run", "main", &marker);
    }
    result
}

fn agent_service(core: &Sender<Input>, lifecycle: LifecycleRef, progress: Arc<crate::watchdog::Progress>) -> Service {
    let (cancel_core, compact_core, follow_core, navigate_core, next_core) =
        (core.clone(), core.clone(), core.clone(), core.clone(), core.clone());
    let (prompt_core, abort_core, resume_core, steer_core) = (core.clone(), core.clone(), core.clone(), core.clone());
    Service::new(protocol::names::AGENT_CONTROLLER)
        .method("cancelQueued", move |args| {
            let entry_id = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&cancel_core, Op::CancelQueued { entry_id })
        })
        .method("compact", move |args| {
            let instructions = request(&args)["customInstructions"].as_str().map(str::to_string);
            call_core(&compact_core, Op::Compact { instructions })
        })
        .method("followUp", move |args| queue(&follow_core, QueueKind::FollowUp, &args))
        .method("navigate", move |args| {
            let req = request(&args);
            call_core(
                &navigate_core,
                Op::Navigate {
                    target_id: req["targetId"].as_str().map(str::to_string),
                    summarize: req["summarize"] == true,
                    instructions: req["customInstructions"].as_str().map(str::to_string),
                    label: req["label"].as_str().map(str::to_string),
                },
            )
        })
        .method("nextRun", move |args| queue(&next_core, QueueKind::NextRun, &args))
        .method("prompt", move |args| run_prompt_method(&prompt_core, &lifecycle, &args))
        .method("requestAbort", move |_| call_core(&abort_core, Op::Abort))
        .method("resume", move |_| {
            let recovered = call_core(&resume_core, Op::Recover)?.unwrap_or_default();
            let response = crate::session_control::resume_response(&recovered, progress.run());
            if let Some(run) = response["operationId"].as_str() {
                progress.wait_until_not(run, None);
            }
            Ok(Some(response))
        })
        .method("steer", move |args| queue(&steer_core, QueueKind::Steer, &args))
}

fn legacy_commands_service(
    mcp: Arc<OnceLock<Arc<mcp::gateway::Gateway>>>,
    ui: Arc<crate::extension_ui::ExtensionUiBridge>,
) -> Service {
    let list_mcp = mcp.clone();
    Service::new(protocol::names::LEGACY_EXTENSION_COMMANDS)
        .method("list", move |_| {
            let listed: Vec<Value> = list_mcp
                .get()
                .map(|_| session_core::commands::command_info(&session_core::commands::mcp_command()))
                .into_iter()
                .collect();
            Ok(Some(listed.into()))
        })
        .method("run", move |args| {
            let text = |i: usize| args.get(i).and_then(Value::as_str).map(str::to_string);
            let (Some(name), Some(cmd_args)) = (text(0), text(1)) else {
                return Err(ServiceError::internal("Extension command service is unavailable"));
            };
            match mcp.get() {
                Some(gateway) if name == "mcp" => {
                    gateway.run_command(&cmd_args, &crate::extension_ui::BridgeUi(ui.clone()));
                    Ok(Some(json!({"notifications": []})))
                }
                _ => Err(ServiceError::internal(format!("Unknown extension command: /{name}"))),
            }
        })
}

fn session_control_service(
    core: &Sender<Input>,
    control_context: Arc<crate::session_control::ControlContext>,
) -> Service {
    let (abort_bash_core, bash_core, forked_core, inspect_core, tree_core, name_core, commands_core) = (
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
    );
    let control = Service::new(protocol::names::SESSION_CONTROL)
        .method("abortBash", move |_| call_core(&abort_bash_core, Op::AbortBash))
        .method("bash", move |args| {
            let command = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            let exclude = args.get(1) == Some(&Value::Bool(true));
            let id = args.get(2).and_then(Value::as_str).map(str::to_string);
            call_core(&bash_core, Op::Bash { command, exclude, id })
        })
        .method("forked", move |args| match args.first().and_then(Value::as_str) {
            Some(target) => call_core(
                &forked_core,
                Op::Forked {
                    target: target.to_string(),
                },
            ),
            None => Ok(None),
        })
        .method("inspect", move |args| match args.first().and_then(Value::as_str) {
            Some("async.pending") => call_core(&inspect_core, Op::AsyncPending),
            other => call_core(
                &inspect_core,
                Op::Inspect {
                    request: other.unwrap_or_default().to_string(),
                    payload: args.get(1).cloned().unwrap_or(Value::Null),
                },
            ),
        })
        .method("readTree", move |_| {
            let entries = call_core(&tree_core, Op::Entries { since: None })?.unwrap_or_default();
            Ok(Some(
                json!({"entries": entries["entries"], "leafId": entries["leafId"]}),
            ))
        })
        .method("setName", move |args| {
            let name = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&name_core, Op::SetName { name })
        })
        .method("listCommands", move |_| {
            let commands = call_core(&commands_core, Op::Commands)?.unwrap_or_default();
            let commands = commands.as_array().map_or(&[][..], Vec::as_slice);
            Ok(Some(session_core::commands::session_command_infos(commands).into()))
        });
    let settings_core = core.clone();
    crate::session_control::members(
        control,
        Arc::new(move |op| session_core::core::call(&settings_core, op)),
        control_context,
    )
}

fn extension_ui_service(ui: Arc<crate::extension_ui::ExtensionUiBridge>) -> Service {
    let poll_ui = ui.clone();
    Service::new(protocol::names::EXTENSION_UI)
        .method("poll", move |args| {
            let after = args.first().and_then(Value::as_u64);
            let wait = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            Ok(Some(poll_ui.poll(after, Duration::from_millis(wait))))
        })
        .method("respond", move |args| {
            if let (Some(id), Some(answer)) = (args.first().and_then(Value::as_str), args.get(1)) {
                ui.respond(id, answer.clone());
            }
            Ok(None)
        })
}

fn models_service(core: &Sender<Input>, models_state: Arc<crate::chord::State>, env: RunEnv) -> Service {
    let (cycle_core, levels_core, select_core, thinking_core, refresh_core) =
        (core.clone(), core.clone(), core.clone(), core.clone(), core.clone());
    let (select_state, thinking_state, refresh_state) =
        (models_state.clone(), models_state.clone(), models_state.clone());
    let (refresh_env, select_env) = (env.clone(), env);
    Service::new(protocol::names::MODELS)
        .method("cycleThinking", move |_| call_core(&cycle_core, Op::CycleThinking))
        .method("getThinkingLevels", move |_| {
            call_core(&levels_core, Op::ThinkingLevels)
        })
        .method("refresh", move |_| {
            let session = call_core(&refresh_core, Op::State)?.unwrap_or_default();
            let env = refresh_env.clone();
            crate::session_control::refresh_models(
                &refresh_state,
                || session_core::model_refresh::refresh_catalogs(&env),
                || {
                    session_core::lane::available_models(&refresh_env)
                        .iter()
                        .filter_map(|m| serde_json::to_value(m).ok())
                        .collect()
                },
                &session,
            )
        })
        .method("select", move |args| {
            let target = request(&args);
            let provider = target["provider"].as_str().unwrap_or_default().to_string();
            let model_id = target["modelId"].as_str().unwrap_or_default().to_string();
            let model = call_core(&select_core, Op::SetModel { provider, model_id })?;
            if let Some(model) = &model {
                save_default_model(&select_env, model);
                select_state.change(|state| {
                    state["configuration"]["model"] = json!({"provider": model["provider"], "modelId": model["id"]});
                });
            }
            Ok(model)
        })
        .method("selectThinking", move |args| {
            let level = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&thinking_core, Op::SetThinking { level: level.clone() })?;
            thinking_state.change(|state| state["configuration"]["thinkingLevel"] = json!(level));
            Ok(None)
        })
        .state("state", models_state)
}

fn save_selected_model(env: &RunEnv, op: &Op, answer: Option<&Value>) {
    let chosen = match (op, answer) {
        (Op::SetModel { .. }, Some(model)) => Some(model),
        (Op::CycleModel, Some(cycled)) => cycled.get("model"),
        _ => None,
    };
    if let Some(model) = chosen {
        save_default_model(env, model);
    }
}

fn run_session_op(core: &Sender<Input>, env: &RunEnv, args: &[Value]) -> ServiceResult {
    let op = Op::from_json(request(args)).map_err(ServiceError::internal)?;
    if let Op::Prompt { message, .. } | Op::PromptAndWait { message, .. } = &op
        && let Some((name, command)) = crate::session_commands::command(message)
    {
        return Ok(Some(crate::session_commands::answer(name, call_core(core, command)?)));
    }
    let answer = call_core(core, op.clone())?;
    save_selected_model(env, &op, answer.as_ref());
    Ok(answer)
}

fn abort_stuck_run(core: &Sender<Input>, watch: &Watch) -> ServiceResult {
    let Some(run) = watch.progress.run() else {
        return Ok(Some(json!({"aborted": false, "reason": "no run in progress"})));
    };
    call_core(core, Op::Abort)?;
    if watch
        .progress
        .wait_until_not(&run, Some(Instant::now() + watch.abort_grace))
    {
        return Ok(Some(json!({"aborted": true, "runId": run, "restarted": false})));
    }
    if let Some(shared) = watch.shared.get().and_then(Weak::upgrade) {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            shared.close_and_exit(RESTART_EXIT_CODE)
        });
    }
    Ok(Some(json!({"aborted": true, "runId": run, "restarted": true})))
}

fn session_ops_service(
    core: &Sender<Input>,
    name_state: Arc<crate::chord::State>,
    watch: Watch,
    env: RunEnv,
) -> Service {
    let (op_core, stuck_core) = (core.clone(), core.clone());
    let health_progress = watch.progress.clone();
    let health_state = watch.health.clone();
    let stuck_after = watch.stuck_after;
    Service::new(protocol::names::SESSION_OPS)
        .method("op", move |args| run_session_op(&op_core, &env, &args))
        .method("checkHealth", move |_| {
            Ok(Some(health_progress.health(jsrt::date::now_ms(), stuck_after)))
        })
        .method("abortStuck", move |_| abort_stuck_run(&stuck_core, &watch))
        .state("name", name_state)
        .state("health", health_state)
        .unlisted()
}

/// The session services in their catalogue order, over the session actor `core`.
pub(super) fn services(
    lane: &Lane,
    models_state: Arc<crate::chord::State>,
    name_state: Arc<crate::chord::State>,
    core: Sender<Input>,
    lifecycle: LifecycleRef,
    watch: Watch,
    extensions: Extensions,
) -> Provider {
    let agent = agent_service(&core, lifecycle, watch.progress.clone());
    let plugins = Service::new(protocol::names::SESSION_PLUGINS).method("reload", |_| Ok(None));
    let legacy = legacy_commands_service(extensions.mcp.clone(), extensions.ui.clone());
    let control = session_control_service(&core, extensions.control.clone());
    let extension_ui = extension_ui_service(extensions.ui);
    let models = models_service(&core, models_state, extensions.env.clone());
    let transcript = Service::new(protocol::names::TRANSCRIPT).state("state", lane.transcript.clone());
    let ops = session_ops_service(&core, name_state, watch, extensions.env);
    Provider {
        services: vec![agent, plugins, legacy, control, extension_ui, models, transcript, ops],
    }
}
