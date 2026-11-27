use super::{CallError, Connection};
use protocol::{Route, RpcTarget};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

/// What `runClient` is asked to do (`ClientCommand`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientCommand {
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub continue_: bool,
    pub prompt: Option<String>,
    pub fork_from_session_id: Option<String>,
    pub no_session: bool,
    /// The prompt's images (`@file` images): `{type: "image", mimeType, data}`.
    pub images: Option<Vec<Json>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientResult {
    List(Vec<(String, String)>),
    Attached {
        server_id: String,
        session_id: String,
    },
    Prompted {
        server_id: String,
        session_id: String,
        text: String,
    },
}

impl ClientResult {
    /// `JSON.stringify(result)`, as `--mode json` prints it.
    pub fn to_json(&self) -> Json {
        match self {
            ClientResult::List(sessions) => json!({"kind": "list", "sessions": sessions.iter()
                .map(|(server, session)| json!({"serverId": server, "sessionId": session})).collect::<Vec<_>>()}),
            ClientResult::Attached { server_id, session_id } => {
                json!({"kind": "attached", "serverId": server_id, "sessionId": session_id})
            }
            ClientResult::Prompted {
                server_id,
                session_id,
                text,
            } => json!({"kind": "prompted", "serverId": server_id, "sessionId": session_id, "text": text}),
        }
    }
}

fn fail(error: CallError) -> String {
    crate::lost::message(&error)
}

/// Told every transcript event of a prompted session (`onEvent`).
pub type EventListener = Arc<dyn Fn(&Json) + Send + Sync>;

/// How a prompted client survives interruptions.
#[derive(Clone, Default)]
pub struct RunOptions {
    /// Set when the user interrupts (SIGINT/SIGTERM): the prompted turn is aborted, and the client still waits for
    /// it to end, as Pi's print mode does.
    pub interrupt: Option<Arc<AtomicBool>>,
    /// The server went away: reach it again (restarting it), for rejoining the session.
    pub reconnect: Option<Arc<dyn Fn() -> Result<Route, String> + Send + Sync>>,
    /// Every transcript event of the prompted session as it arrives (`onEvent`).
    pub on_event: Option<EventListener>,
    /// A Rust addition: the client's own session options (`ultron durable --model ...`), sent with each attach so
    /// the session's worker starts on them on a server whose workers start on other options.
    pub session_options: Option<Json>,
}

/// How long a client tries to rejoin its session after losing its worker or server.
pub const REJOIN_TIMEOUT: Duration = Duration::from_secs(120);

/// The call failed because the connection or the session's worker went away: the session is durable, rejoin it.
/// Any disconnect counts here, also one [`crate::lost::is_lost`] would not describe as a lost server.
fn must_rejoin(error: &CallError) -> bool {
    match error {
        CallError::Disconnected(_) => true,
        CallError::Server(error) => protocol::names::is_lost_code(&error.code),
    }
}

fn interrupted(options: &RunOptions) -> bool {
    options.interrupt.as_ref().is_some_and(|i| i.load(Ordering::SeqCst))
}

fn ops(connection: &Connection, session: &RpcTarget, op: Json) -> Result<Option<Json>, CallError> {
    connection.call(session.clone(), protocol::names::SESSION_OPS, "op", json!([op]))
}

/// Rejoin `session_id` after its worker or server went away: attach (the server re-attaches clients of a restarted
/// worker by itself; attaching again is harmless), let the worker recover what was interrupted, follow the
/// session until it is idle with nothing queued, and answer its last assistant text.
pub fn rejoin(connection: &Connection, session_id: &str, options: &RunOptions) -> Result<String, CallError> {
    let deadline = Instant::now() + REJOIN_TIMEOUT;
    let session = attach_with_retry(connection, session_id, deadline, options)?;
    let (transcript, boundary) = subscribe_boundaries(connection, &session)?;
    let idle = wait_idle(connection, &session, &boundary, deadline, options);
    cleanup_subscription(connection, &session, &transcript);
    idle?;
    let text = ops(connection, &session, json!({"op": "lastAssistantText"}))?;
    Ok(text.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default())
}

fn attach_with_retry(
    connection: &Connection,
    session_id: &str,
    deadline: Instant,
    options: &RunOptions,
) -> Result<RpcTarget, CallError> {
    let server = connection.server_target();
    loop {
        match try_attach_session(connection, &server, session_id, options) {
            Ok(session) => return Ok(session),
            Err(CallError::Server(error)) if error.code == protocol::names::SESSION_NOT_FOUND_CODE => {
                return Err(CallError::Server(error));
            }
            Err(error) if connection.is_closed() || Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn try_attach_session(
    connection: &Connection,
    server: &RpcTarget,
    session_id: &str,
    options: &RunOptions,
) -> Result<RpcTarget, CallError> {
    let args = match &options.session_options {
        Some(session_options) => json!([session_id, session_options]),
        None => json!([session_id]),
    };
    connection.call(server.clone(), protocol::names::SESSION_MANAGEMENT, "attach", args)?;
    connection.wait_attachment(|a| a.is_some_and(|a| a.session_id == session_id))?;
    connection.session_target()
}

fn subscribe_boundaries(
    connection: &Connection,
    session: &RpcTarget,
) -> Result<(String, mpsc::Receiver<()>), CallError> {
    let (boundaries, boundary) = mpsc::channel::<()>();
    let transcript = connection.subscribe(session.clone(), protocol::names::TRANSCRIPT)?;
    connection.listen(
        &transcript,
        Box::new(move |_, value| {
            if matches!(value["event"]["type"].as_str(), Some("run_end" | "run_suspend")) {
                let _ = boundaries.send(());
            }
        }),
    );
    Ok((transcript, boundary))
}

fn cleanup_subscription(connection: &Connection, session: &RpcTarget, subscription: &str) {
    connection.unlisten(subscription);
    if let Ok(id) = connection.start_unsubscribe(session.clone(), subscription) {
        let _ = connection.wait(&id);
    }
    connection.forget(subscription);
}

/// Recover the rejoined session and wait until it is idle with nothing queued: its state is read again at each run
/// boundary (`boundary`); an interrupt asks the run to abort.
fn wait_idle(
    connection: &Connection,
    session: &RpcTarget,
    boundary: &mpsc::Receiver<()>,
    deadline: Instant,
    options: &RunOptions,
) -> Result<(), CallError> {
    ops(connection, session, json!({"op": "recover"}))?;
    let mut abort_sent = false;
    loop {
        let state = ops(connection, session, json!({"op": "state"}))?.unwrap_or_default();
        let (idle, checking) = session_is_idle(&state, abort_sent);
        if idle {
            return Ok(());
        }
        wait_for_boundary(
            connection,
            session,
            boundary,
            deadline,
            options,
            checking,
            &mut abort_sent,
        )?;
    }
}

fn session_is_idle(state: &Json, abort_sent: bool) -> (bool, bool) {
    let checking = state["isChecking"] == true;
    let streaming = state["isStreaming"] == true;
    let pending = state["pendingMessageCount"].as_u64().unwrap_or(0) > 0;
    let busy = checking || streaming || pending;
    (!busy || (abort_sent && !streaming), checking)
}

fn wait_for_boundary(
    connection: &Connection,
    session: &RpcTarget,
    boundary: &mpsc::Receiver<()>,
    deadline: Instant,
    options: &RunOptions,
    checking: bool,
    abort_sent: &mut bool,
) -> Result<(), CallError> {
    loop {
        if maybe_request_abort(connection, session, options, abort_sent)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CallError::Disconnected(
                "The rejoined session did not become idle".into(),
            ));
        }
        match boundary.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) if connection.is_closed() || checking => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(CallError::Disconnected("The transcript subscription ended".into()));
            }
        }
    }
}

fn maybe_request_abort(
    connection: &Connection,
    session: &RpcTarget,
    options: &RunOptions,
    abort_sent: &mut bool,
) -> Result<bool, CallError> {
    if !interrupted(options) || *abort_sent {
        return Ok(false);
    }
    *abort_sent = true;
    connection.call(
        session.clone(),
        protocol::names::AGENT_CONTROLLER,
        "requestAbort",
        json!([]),
    )?;
    Ok(true)
}

/// `runClient` against one server route.
pub fn run_client(command: &ClientCommand, route: &Route) -> Result<ClientResult, String> {
    run_client_with(command, route, &RunOptions::default())
}

/// [`run_client`] that aborts on interrupt and rejoins its session when the worker or the server goes away.
pub fn run_client_with(command: &ClientCommand, route: &Route, options: &RunOptions) -> Result<ClientResult, String> {
    let connection = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
    run_client_on(command, connection, options)
}

/// [`run_client_with`] over an established connection (a Unix socket's, or the Radius relay's); the connection is
/// disconnected when the run ends.
pub fn run_client_on(
    command: &ClientCommand,
    connection: Arc<Connection>,
    options: &RunOptions,
) -> Result<ClientResult, String> {
    run_client_reopening(command, connection, options, None)
}

/// Opens a new connection to the server with the Session attached again, when the connection dropped mid-prompt
/// (a Radius route's reconnector, [`crate::radius::RadiusReopen`]).
pub type Reopen<'a> = &'a (dyn Fn(&str) -> Result<Arc<Connection>, String> + Send + Sync);

/// [`run_client_on`] whose prompt survives its connection dropping: `reopen` brings the connection back, and the
/// client rejoins the Session on it (follows it until idle and answers its last assistant text).
pub fn run_client_reopening(
    command: &ClientCommand,
    connection: Arc<Connection>,
    options: &RunOptions,
    reopen: Option<Reopen<'_>>,
) -> Result<ClientResult, String> {
    let route_server_id = connection.server_id.clone();
    let (subscriptions, created, outcome) = run_operation(command, &connection, options);
    let cleanup_error = remove_temporary_session(&connection, &created, command.no_session);
    let outcome = match outcome {
        Err(Failure::Lost { session_id, error }) => {
            recover_lost(&connection, &route_server_id, &session_id, error, options, reopen)
        }
        Err(Failure::Other(error)) => Err(error),
        Ok(result) => Ok(result),
    };
    unsubscribe_all(&connection, &subscriptions);
    connection.disconnect();
    match (outcome, cleanup_error) {
        (Err(error), _) => Err(error),
        (Ok(_), Some(error)) => Err(error),
        (Ok(result), None) => Ok(result),
    }
}

fn remove_temporary_session(
    connection: &Connection,
    created: &Result<Option<String>, String>,
    no_session: bool,
) -> Option<String> {
    if !no_session {
        return None;
    }
    let Ok(Some(created)) = created else {
        return None;
    };
    let target = connection.server_target();
    let attached = connection.attachment().is_some_and(|a| &a.session_id == created);
    match connection.call(target, protocol::names::SESSION_MANAGEMENT, "remove", json!([created])) {
        Ok(_) if attached => {
            let _ = connection.wait_attachment(|a| a.is_none());
            None
        }
        Ok(_) => None,
        Err(error) => Some(error.to_string()),
    }
}

fn unsubscribe_all(connection: &Connection, subscriptions: &[(String, bool)]) {
    let mut calls = Vec::new();
    for (subscription, session) in subscriptions {
        let target = if *session {
            connection.session_target()
        } else {
            Ok(connection.server_target())
        };
        if let Ok(target) = target {
            calls.push((
                target,
                protocol::names::CHORD_SERVICE,
                "unsubscribe",
                json!([subscription]),
            ));
        }
    }
    for id in connection.start_all(calls).unwrap_or_default() {
        let _ = connection.wait(&id);
    }
}

/// The prompted session's worker or server went away mid-prompt, or the operation failed.
enum Failure {
    Lost { session_id: String, error: CallError },
    Other(String),
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Failure::Other(error)
    }
}

/// Rejoin after a lost prompt: on the same connection when it lives (the worker restarted), else on a new
/// connection (`reopen`ed over the Radius relay, or to the restarted server). Answers the prompted result.
fn recover_lost(
    connection: &Arc<Connection>,
    server_id: &str,
    session_id: &str,
    error: CallError,
    options: &RunOptions,
    reopen: Option<Reopen<'_>>,
) -> Result<ClientResult, String> {
    let rejoined = if !connection.is_closed() {
        rejoin(connection, session_id, options)
    } else if let Some(reopen) = reopen {
        let fresh = reopen(session_id)?;
        let rejoined = rejoin(&fresh, session_id, options);
        fresh.disconnect();
        rejoined
    } else if let Some(reconnect) = &options.reconnect {
        let route = reconnect()?;
        let fresh = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
        let rejoined = rejoin(&fresh, session_id, options);
        fresh.disconnect();
        rejoined
    } else {
        Err(error)
    };
    rejoined
        .map(|text| ClientResult::Prompted {
            server_id: server_id.to_string(),
            session_id: session_id.to_string(),
            text,
        })
        .map_err(fail)
}

type Operation = (
    Vec<(String, bool)>,
    Result<Option<String>, String>,
    Result<ClientResult, Failure>,
);

fn run_operation(command: &ClientCommand, connection: &Arc<Connection>, options: &RunOptions) -> Operation {
    let mut subscriptions = Vec::new();
    let mut created = Ok(None);
    let outcome = operation(command, connection, &mut subscriptions, &mut created, options);
    (subscriptions, created, outcome)
}

/// Wait for a prompt's response; an interrupt asks the worker to abort the run (once) and keeps waiting.
fn await_prompt(
    connection: &Connection,
    session: &RpcTarget,
    request: &str,
    options: &RunOptions,
) -> Result<Option<Json>, CallError> {
    let mut abort_sent = false;
    loop {
        if let Some(result) = connection.wait_for(request, Duration::from_millis(100)) {
            return result;
        }
        if interrupted(options) && !abort_sent {
            abort_sent = true;
            let _ = connection.start(
                session.clone(),
                protocol::names::AGENT_CONTROLLER,
                "requestAbort",
                json!([]),
            );
        }
    }
}

fn operation(
    command: &ClientCommand,
    connection: &Arc<Connection>,
    subscriptions: &mut Vec<(String, bool)>,
    created: &mut Result<Option<String>, String>,
    options: &RunOptions,
) -> Result<ClientResult, Failure> {
    let server = connection.server_target();
    let server_id = connection.server_id.clone();
    let sessions = subscribe_server_services(connection, &server, subscriptions)?;
    if command.session_id.is_none() && command.prompt.is_none() {
        return Ok(list_sessions(&server_id, &sessions));
    }
    let session_id = resolve_session_id(command, connection, &server, &sessions, created)?;
    let (session, transcript) = attach_and_subscribe_session(connection, &server, &session_id, options, subscriptions)?;
    let Some(prompt) = &command.prompt else {
        return Ok(ClientResult::Attached { server_id, session_id });
    };
    execute_prompt(
        command,
        connection,
        &session,
        &server_id,
        &session_id,
        &transcript,
        prompt,
        options,
    )
}

fn subscribe_server_services(
    connection: &Arc<Connection>,
    server: &RpcTarget,
    subscriptions: &mut Vec<(String, bool)>,
) -> Result<Vec<Json>, Failure> {
    let mut started = Vec::new();
    for service in [
        protocol::names::SESSION_DIRECTORY,
        protocol::names::SESSION_MANAGEMENT,
        protocol::names::PRESENTATION_PLUGINS,
    ] {
        started.push(connection.start_subscribe(server.clone(), service).map_err(fail)?);
    }
    for (subscription, request) in &started {
        connection.finish_subscribe(subscription, request).map_err(fail)?;
        subscriptions.push((subscription.clone(), false));
    }
    let directory = &started[0].0;
    Ok(connection
        .state(directory, "state")
        .map_or_default(|s| s["sessions"].as_array().cloned().unwrap_or_default()))
}

fn list_sessions(server_id: &str, sessions: &[Json]) -> ClientResult {
    let mut list: Vec<(String, String)> = sessions
        .iter()
        .map(|s| {
            (
                server_id.to_string(),
                s["sessionId"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    list.sort();
    ClientResult::List(list)
}

fn resolve_session_id(
    command: &ClientCommand,
    connection: &Arc<Connection>,
    server: &RpcTarget,
    sessions: &[Json],
    created: &mut Result<Option<String>, String>,
) -> Result<String, Failure> {
    let mut session_id = command.session_id.clone();
    if session_id.is_none() && command.continue_ {
        session_id = newest_session_id(sessions);
    }
    let create_options = build_create_options(command);
    match session_id {
        None => {
            let summary = management_call(connection, server, "create", json!([create_options]))?.unwrap_or_default();
            let id = summary["sessionId"].as_str().unwrap_or_default().to_string();
            *created = Ok(Some(id.clone()));
            Ok(id)
        }
        Some(id) => {
            ensure_named_session(&id, command, connection, server, sessions, &create_options, created)?;
            Ok(id)
        }
    }
}

fn management_call(
    connection: &Arc<Connection>,
    server: &RpcTarget,
    member: &str,
    args: Json,
) -> Result<Option<Json>, Failure> {
    connection
        .call(server.clone(), protocol::names::SESSION_MANAGEMENT, member, args)
        .map_err(fail)
        .map_err(Failure::Other)
}

fn newest_session_id(sessions: &[Json]) -> Option<String> {
    let mut candidates = sessions.to_vec();
    candidates.sort_by(|a, b| {
        let modified = |s: &Json| s["modifiedAt"].as_f64().unwrap_or(0.0);
        modified(b)
            .partial_cmp(&modified(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a["sessionId"].as_str().cmp(&b["sessionId"].as_str()))
    });
    candidates
        .first()
        .and_then(|s| s["sessionId"].as_str())
        .map(str::to_string)
}

fn build_create_options(command: &ClientCommand) -> Json {
    let mut create_options = json!({});
    if let Some(fork) = &command.fork_from_session_id {
        create_options["forkFromSessionId"] = json!(fork);
    }
    if let Some(name) = &command.name {
        create_options["name"] = json!(name);
    }
    create_options
}

fn ensure_named_session(
    id: &str,
    command: &ClientCommand,
    connection: &Arc<Connection>,
    server: &RpcTarget,
    sessions: &[Json],
    create_options: &Json,
    created: &mut Result<Option<String>, String>,
) -> Result<(), Failure> {
    if sessions.iter().any(|s| s["sessionId"] == json!(id)) {
        if command.fork_from_session_id.is_some() {
            return Err(format!("Cannot fork into existing Session {id}").into());
        }
        if let Some(name) = &command.name {
            management_call(connection, server, "rename", json!([id, name]))?;
        }
        return Ok(());
    }
    if command.prompt.is_none() {
        return Err(format!("No discovered server contains session {id}").into());
    }
    let mut ordered = json!({"id": id});
    for (key, value) in create_options.as_object().into_iter().flatten() {
        ordered[key] = value.clone();
    }
    management_call(connection, server, "create", json!([ordered]))?;
    *created = Ok(Some(id.to_string()));
    Ok(())
}

fn attach_and_subscribe_session(
    connection: &Arc<Connection>,
    server: &RpcTarget,
    session_id: &str,
    options: &RunOptions,
    subscriptions: &mut Vec<(String, bool)>,
) -> Result<(RpcTarget, String), Failure> {
    let session = attach_session(connection, server, session_id, options)?;
    let transcript = subscribe_session_services(connection, &session, subscriptions)?;
    Ok((session, transcript))
}

fn attach_session(
    connection: &Arc<Connection>,
    server: &RpcTarget,
    session_id: &str,
    options: &RunOptions,
) -> Result<RpcTarget, Failure> {
    connection.prepare_session(session_id).map_err(fail)?;
    let attach = match &options.session_options {
        Some(session_options) => json!([session_id, session_options]),
        None => json!([session_id]),
    };
    management_call(connection, server, "attach", attach)?;
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == session_id))
        .map_err(fail)?;
    connection.session_target().map_err(fail).map_err(Failure::Other)
}

fn subscribe_session_services(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    subscriptions: &mut Vec<(String, bool)>,
) -> Result<String, Failure> {
    let catalogue = connection
        .start(session.clone(), protocol::names::CHORD_SERVICE, "catalogue", json!([]))
        .map_err(fail)?;
    let mut started = Vec::new();
    for service in [
        protocol::names::MODELS,
        protocol::names::AGENT_CONTROLLER,
        protocol::names::TRANSCRIPT,
    ] {
        started.push(connection.start_subscribe(session.clone(), service).map_err(fail)?);
    }
    connection.wait(&catalogue).map_err(fail)?;
    for (subscription, request) in &started {
        connection.finish_subscribe(subscription, request).map_err(fail)?;
        subscriptions.push((subscription.clone(), true));
    }
    Ok(started[2].0.clone())
}

#[allow(clippy::too_many_arguments, reason = "internal helper decomposed from operation")]
fn execute_prompt(
    command: &ClientCommand,
    connection: &Arc<Connection>,
    session: &RpcTarget,
    server_id: &str,
    session_id: &str,
    transcript: &str,
    prompt: &str,
    options: &RunOptions,
) -> Result<ClientResult, Failure> {
    if connection
        .state(transcript, "state")
        .is_none_or(|s| s["snapshot"].is_null())
    {
        return Err(Failure::Other("Transcript has no initialized snapshot".into()));
    }
    let received = listen_transcript_events(connection, transcript, options.on_event.clone());
    let response = match send_prompt(
        connection,
        session,
        session_id,
        prompt,
        command.images.as_ref(),
        options,
    ) {
        Ok(response) => response,
        Err(failure) => {
            connection.unlisten(transcript);
            return Err(failure);
        }
    };
    if response["command"].is_string() {
        connection.unlisten(transcript);
        return Ok(ClientResult::Prompted {
            server_id: server_id.to_string(),
            session_id: session_id.to_string(),
            text: response["text"].as_str().unwrap_or_default().to_string(),
        });
    }
    if response["accepted"] == true {
        return await_accepted_prompt(
            connection, session, server_id, session_id, transcript, &response, &received,
        );
    }
    connection.unlisten(transcript);
    Err(Failure::Other(
        response["error"]["message"]
            .as_str()
            .unwrap_or("Prompt was not accepted")
            .to_string(),
    ))
}

fn listen_transcript_events(
    connection: &Arc<Connection>,
    transcript: &str,
    on_event: Option<EventListener>,
) -> mpsc::Receiver<Json> {
    let (events, received) = mpsc::channel::<Json>();
    connection.listen(
        transcript,
        Box::new(move |_, value| {
            if !value["event"].is_null() {
                if let Some(on_event) = &on_event {
                    on_event(&value["event"]);
                }
                let _ = events.send(value["event"].clone());
            }
        }),
    );
    received
}

fn send_prompt(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    session_id: &str,
    prompt: &str,
    images: Option<&Vec<Json>>,
    options: &RunOptions,
) -> Result<Json, Failure> {
    let images = images.cloned().map_or(Json::Null, Json::Array);
    let response = connection
        .start(
            session.clone(),
            protocol::names::AGENT_CONTROLLER,
            "prompt",
            json!([{"message": prompt, "images": images}]),
        )
        .and_then(|request| await_prompt(connection, session, &request, options));
    match response {
        Err(error) if must_rejoin(&error) => Err(Failure::Lost {
            session_id: session_id.to_string(),
            error,
        }),
        Err(error) => Err(Failure::Other(fail(error))),
        Ok(Some(response)) => Ok(response),
        Ok(None) => Err(Failure::Other("Agent prompt returned no response".into())),
    }
}

fn absorb_transcript_event(event: &Json, completed: &mut HashMap<String, String>, ended: &mut Vec<String>) {
    let run = event["runId"].as_str().map(str::to_string);
    if event["type"] == "message_end"
        && event["message"]["role"] == "assistant"
        && let Some(run) = &run
    {
        completed.insert(run.clone(), protocol::pi_message::text(&event["message"]));
    }
    if matches!(event["type"].as_str(), Some("run_end" | "run_suspend"))
        && let Some(run) = run
    {
        ended.push(run);
    }
}

#[allow(clippy::too_many_arguments, reason = "internal helper decomposed from operation")]
fn await_accepted_prompt(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    server_id: &str,
    session_id: &str,
    transcript: &str,
    response: &Json,
    received: &mpsc::Receiver<Json>,
) -> Result<ClientResult, Failure> {
    let mut completed: HashMap<String, String> = HashMap::new();
    let mut ended = Vec::new();
    let operation = response["operationId"].as_str().unwrap_or_default().to_string();
    if let Err(failure) =
        wait_for_operation_end(connection, session_id, &operation, received, &mut completed, &mut ended)
    {
        connection.unlisten(transcript);
        return Err(failure);
    }
    let final_run = finish_accepted_runs(
        connection,
        session,
        &operation,
        response,
        received,
        &mut completed,
        &mut ended,
    );
    connection.unlisten(transcript);
    drain_transcript_events(received, &mut completed, &mut ended);
    if let Some(message) = response["error"]["message"].as_str() {
        return Err(Failure::Other(message.to_string()));
    }
    Ok(ClientResult::Prompted {
        server_id: server_id.to_string(),
        session_id: session_id.to_string(),
        text: completed.get(&final_run).cloned().unwrap_or_default(),
    })
}

fn wait_for_operation_end(
    connection: &Arc<Connection>,
    session_id: &str,
    operation: &str,
    received: &mpsc::Receiver<Json>,
    completed: &mut HashMap<String, String>,
    ended: &mut Vec<String>,
) -> Result<(), Failure> {
    while !ended.iter().any(|r| r == operation) {
        match received.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => absorb_transcript_event(&event, completed, ended),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Err(error) = connection.wait_attachment(|_| true) {
                    return Err(Failure::Lost {
                        session_id: session_id.to_string(),
                        error,
                    });
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn finish_accepted_runs(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    operation: &str,
    response: &Json,
    received: &mpsc::Receiver<Json>,
    completed: &mut HashMap<String, String>,
    ended: &mut Vec<String>,
) -> String {
    if !response["error"].is_null() {
        return operation.to_string();
    }
    follow_event_runs(connection, session, received, &mut |e| {
        absorb_transcript_event(e, completed, ended);
    });
    ended
        .iter()
        .position(|r| r == operation)
        .and_then(|index| ended[index + 1..].last())
        .cloned()
        .unwrap_or_else(|| operation.to_string())
}

fn drain_transcript_events(
    received: &mpsc::Receiver<Json>,
    completed: &mut HashMap<String, String>,
    ended: &mut Vec<String>,
) {
    while let Ok(event) = received.try_recv() {
        absorb_transcript_event(&event, completed, ended);
    }
}

/// `followEventRuns`: while the worker reports root-owned work that can re-invoke the model, keep waiting.
fn follow_event_runs(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    received: &mpsc::Receiver<Json>,
    absorb: &mut dyn FnMut(&Json),
) {
    let Ok(control) = connection.subscribe(session.clone(), protocol::names::SESSION_CONTROL) else {
        return;
    };
    let mut running: Option<String> = None;
    let mut take = |event: Json, running: &mut Option<String>| {
        match event["type"].as_str() {
            Some("run_start") => *running = event["runId"].as_str().map(str::to_string),
            Some("run_end" | "run_suspend") => *running = None,
            _ => {}
        }
        absorb(&event);
    };
    loop {
        while let Ok(event) = received.try_recv() {
            take(event, &mut running);
        }
        if running.is_none() {
            let status = connection.call(
                session.clone(),
                protocol::names::SESSION_CONTROL,
                "inspect",
                json!(["async.pending", {}]),
            );
            if !matches!(status, Ok(Some(status)) if status["pending"] == true) {
                break;
            }
        }
        if let Ok(event) = received.recv_timeout(Duration::from_millis(200)) {
            take(event, &mut running);
        }
    }
    if let Ok(id) = connection.start_unsubscribe(session.clone(), &control) {
        let _ = connection.wait(&id);
    }
    connection.forget(&control);
}
