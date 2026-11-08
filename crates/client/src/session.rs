//! A client's link to one session, as `rpc-native.ts` and `client-tui.ts` hold it: a server (one this process starts
//! or activates, or a remote route), a client connection, the session attached, its services subscribed, and its
//! transcript's lane-watch events forwarded to the caller. Operations go to the session worker's
//! `ultron.session-ops.op`; switching sessions re-attaches, as TypeScript clients do.
//!
//! The link never gives up on its session: a supervisor watches the connection and the attachment. When the
//! session's worker restarts (the server re-attaches this client under a new attachment) or the server itself goes
//! away (a new server generation is started, which brings up a new coordinator), the link re-attaches, subscribes
//! again and reports `{"type": "connection", "state": "reconnecting" | "rejoined" | "lost"}` to its watcher.

use crate::radius::{ConnectionState, RadiusClientReconnect, ReconnectClient};
use crate::runtime::{CallError, Connection};
use serde_json::{Value, json};
use server::radius_relay::RelayAuthResolver;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A session call's answer: its value, or the error as text ([`crate::lost::message`]).
pub type CallResult = Result<Option<Value>, String>;

/// Where a link forwards the attached session's lane-watch events.
pub type WatchSink = Arc<dyn Fn(Value) + Send + Sync>;

/// How long the supervisor and a refused call wait for a change before they look again: a bound, since they are
/// woken by the change itself (the connection closing or its attachment moving, or the link's own state).
const SUPERVISE: Duration = Duration::from_millis(100);
/// How long a link keeps trying to rejoin before it reports the session lost.
const REJOIN_TIMEOUT: Duration = crate::runtime::REJOIN_TIMEOUT;

/// The `reconnecting` reason when the attached session's worker restarted under the link.
pub const WORKER_RESTARTED: &str = "The session worker restarted";

/// A `connection` watch event.
pub fn connection_event(state: &str, message: Option<&str>) -> Value {
    json!({"type": "connection", "state": state, "message": message})
}

/// A server this client reaches but does not run (`ClientRuntimeRoute`): a Unix socket, or a server hosted through
/// the Radius gateway.
#[derive(Clone)]
pub enum RemoteRoute {
    Unix(protocol::Route),
    Radius {
        server_id: String,
        auth: Arc<dyn RelayAuthResolver>,
    },
}

impl RemoteRoute {
    pub fn server_id(&self) -> &str {
        match self {
            RemoteRoute::Unix(route) => &route.server_id,
            RemoteRoute::Radius { server_id, .. } => server_id,
        }
    }

    /// `unix` or `radius`.
    pub fn transport(&self) -> &'static str {
        match self {
            RemoteRoute::Unix(_) => "unix",
            RemoteRoute::Radius { .. } => "radius",
        }
    }

    pub fn socket_path(&self) -> Option<&std::path::Path> {
        match self {
            RemoteRoute::Unix(route) => Some(&route.path),
            RemoteRoute::Radius { .. } => None,
        }
    }

    pub fn connect(&self) -> Result<Arc<Connection>, CallError> {
        match self {
            RemoteRoute::Unix(route) => Connection::connect(&route.path, &route.server_id),
            RemoteRoute::Radius { server_id, auth } => Connection::connect_radius(server_id, auth.as_ref()),
        }
    }
}

/// Stops a server this process runs.
pub type Stop = Arc<dyn Fn() + Send + Sync>;

/// A server [`Start`] brought up: its route, and how to stop it (`None` for one this client does not own).
pub struct Started {
    pub route: protocol::Route,
    pub stop: Option<Stop>,
}

/// Starts the server a link uses, and starts it again when it went away: this process's own server generation, or
/// a detached server activated when it is not running (a client of the durable instance, which never stops it).
pub type Start = Arc<dyn Fn() -> Result<Started, String> + Send + Sync>;

enum Origin {
    /// Brought up by `start`; every attach carries `session_options` (the options a durable client's sessions start
    /// their workers on).
    Started {
        start: Start,
        session_options: Option<Value>,
    },
    Remote(RemoteRoute),
}

/// The session a remote client opens (`prepareClientSession`): the one named, the most recently modified one, a
/// new one (a fork of `fork` when given); `name` names a new one and renames an existing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSelection {
    pub session_id: Option<String>,
    pub continue_: bool,
    pub name: Option<String>,
    pub fork: Option<String>,
}

/// What [`select_remote_session`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Selected {
    /// Open this existing session (renaming it first when `rename`).
    Existing { session_id: String, rename: Option<String> },
    /// Create a session with these `ultron.session-management.create` options.
    Create(Value),
}

/// `prepareClientSession`'s choice among the server's sessions (`describe` summaries).
pub fn select_remote_session(
    selection: &RemoteSelection,
    sessions: &[Value],
    radius: bool,
) -> Result<Selected, String> {
    let existing = |id: &str| Selected::Existing {
        session_id: id.to_string(),
        rename: selection.name.clone(),
    };
    if let Some(id) = &selection.session_id {
        if sessions.iter().any(|s| s["sessionId"] == json!(id)) {
            return Ok(existing(id));
        }
        if radius {
            return Err(format!("Remote server does not contain Session {id}"));
        }
        let mut options = json!({"id": id});
        if let Some(name) = &selection.name {
            options["name"] = json!(name);
        }
        return Ok(Selected::Create(options));
    }
    if selection.continue_ {
        let text = |s: &Value, key: &str| s[key].as_str().unwrap_or_default().to_string();
        let newest = sessions.iter().min_by(|a, b| {
            let modified = |s: &Value| s["modifiedAt"].as_f64().unwrap_or(0.0);
            modified(b)
                .partial_cmp(&modified(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| text(a, "serverId").cmp(&text(b, "serverId")))
                .then_with(|| text(a, "sessionId").cmp(&text(b, "sessionId")))
        });
        if let Some(newest) = newest {
            return Ok(existing(&text(newest, "sessionId")));
        }
    }
    let mut options = json!({});
    if let Some(fork) = &selection.fork {
        options["forkFromSessionId"] = json!(fork);
    }
    if let Some(name) = &selection.name {
        options["name"] = json!(name);
    }
    Ok(Selected::Create(options))
}

/// A Radius recovery's watch events (`ServerConnectionState` and `SessionAttachmentState` of client-tui.ts).
pub fn radius_connection_event(state: ConnectionState) -> Value {
    let state = match state {
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Disconnected => "disconnected",
    };
    json!({"type": "radius_connection", "state": state})
}

pub fn radius_attachment_event(state: &str, session_id: &str) -> Value {
    json!({"type": "radius_attachment", "state": state, "sessionId": session_id})
}

struct State {
    /// Stops the server generation this process runs (none for a server it does not own).
    server: Option<Stop>,
    connection: Arc<Connection>,
    session_id: String,
    /// The attachment the subscriptions belong to.
    attachment_id: Option<String>,
    subscriptions: Vec<String>,
    transcript: Option<String>,
    /// A reconnection is under way (operations wait for it).
    reconnecting: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    origin: Origin,
    events: WatchSink,
    /// A Radius route's reconnector (`RadiusClientReconnect`), and whether it is recovering the connection now.
    radius: Mutex<Option<Arc<RadiusClientReconnect>>>,
    radius_recovering: AtomicBool,
    /// A reconnection attempt is under way (`connecting`).
    radius_connecting: AtomicBool,
    /// A remote Unix route's loss was reported (it has no reconnect).
    lost_reported: AtomicBool,
    closing: AtomicBool,
    /// Serializes attaching (the supervisor and the owner's `attach`).
    attaching: Mutex<()>,
    /// The operation [`SessionLink::keep_op`] keeps: run again on every session this link attaches (a switch, a
    /// restarted worker, a new server generation), which starts without it.
    kept_op: Mutex<Option<Value>>,
}

/// The link as another thread follows it ([`SessionLink::handle`]).
#[derive(Clone)]
pub struct LinkHandle {
    shared: Arc<Shared>,
}

impl LinkHandle {
    pub fn connection(&self) -> Arc<Connection> {
        self.shared.connection()
    }

    pub fn session_id(&self) -> String {
        self.shared.lock().session_id.clone()
    }
}

pub struct SessionLink {
    shared: Arc<Shared>,
    /// Sessions this link created (removed on close when asked).
    created: Vec<String>,
}

impl Shared {
    /// A call of the attached session, sent again once the link rejoined when it was refused before reaching a worker.
    fn session_call(&self, service: &str, member: &str, args: Value) -> CallResult {
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        loop {
            self.wait_reconnected();
            let connection = self.connection();
            if connection.is_closed() && !self.lost_reported.load(Ordering::SeqCst) && Instant::now() < deadline {
                // Not sent: wait for the supervisor to reconnect.
                self.wait_link_change();
                continue;
            }
            let result = connection
                .session_target()
                .and_then(|session| connection.call(session, service, member, args.clone()));
            match result {
                // Refused before it reached a worker: send it again once the link rejoined.
                Err(CallError::Server(error))
                    if error.code == protocol::names::RECONNECTING_CODE && Instant::now() < deadline =>
                {
                    connection.wait_attachment_change(connection.attachment().as_ref(), SUPERVISE);
                }
                other => return other.map_err(crate::lost::message),
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        jsrt::sync::lock(&self.state)
    }

    fn connection(&self) -> Arc<Connection> {
        self.lock().connection.clone()
    }

    fn management(&self, member: &str, args: Value) -> Result<Option<Value>, String> {
        let connection = self.connection();
        connection
            .call(
                connection.server_target(),
                protocol::names::SESSION_MANAGEMENT,
                member,
                args,
            )
            .map_err(crate::lost::message)
    }

    /// Drop this link's subscriptions (unsubscribing them while the attachment still serves them).
    fn unsubscribe(&self, state: &mut State) {
        let connection = state.connection.clone();
        if let Some(transcript) = state.transcript.take() {
            connection.unlisten(&transcript);
        }
        for subscription in &state.subscriptions {
            connection.unlisten(subscription);
        }
        let current = connection.attachment().map(|a| a.attachment_id);
        let session = connection
            .session_target()
            .ok()
            .filter(|_| current.is_some() && current == state.attachment_id);
        let pending: Vec<String> = state
            .subscriptions
            .drain(..)
            .filter_map(|s| {
                connection.forget(&s);
                session
                    .as_ref()
                    .and_then(|session| connection.start_unsubscribe(session.clone(), &s).ok())
            })
            .collect();
        for id in pending {
            let _ = connection.wait(&id);
        }
        state.attachment_id = None;
    }

    /// Attach to session `id` (replacing the current one): its services subscribed, its transcript followed.
    fn attach(&self, id: &str) -> Result<(), String> {
        let _attaching = self.attaching.lock().unwrap_or_else(|e| e.into_inner());
        let connection = {
            let mut state = self.lock();
            self.unsubscribe(&mut state);
            state.connection.clone()
        };
        connection.prepare_session(id).map_err(crate::lost::message)?;
        let options = match &self.origin {
            Origin::Started { session_options, .. } => session_options.clone(),
            Origin::Remote(_) => None,
        };
        let args = match options {
            Some(options) => json!([id, options]),
            None => json!([id]),
        };
        self.management("attach", args)?;
        connection
            .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
            .map_err(crate::lost::message)?;
        self.subscribe(&connection, id)?;
        // The session's worker starts without the kept operation: run it (best effort; the caller keeps it again
        // when it changes).
        let kept = self.kept_op.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(op) = kept {
            let _ = connection
                .session_target()
                .and_then(|session| connection.call(session, protocol::names::SESSION_OPS, "op", json!([op])));
        }
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(reconnector) = reconnector {
            reconnector.attachment_changed(Some(id));
        }
        Ok(())
    }

    /// Subscribe the attached session's services and follow its transcript.
    fn subscribe(&self, connection: &Arc<Connection>, id: &str) -> Result<(), String> {
        let attachment = connection.attachment().map(|a| a.attachment_id);
        let session = connection.session_target().map_err(crate::lost::message)?;
        let mut started = Vec::new();
        for service in [
            protocol::names::MODELS,
            protocol::names::AGENT_CONTROLLER,
            protocol::names::TRANSCRIPT,
        ] {
            started.push(
                connection
                    .start_subscribe(session.clone(), service)
                    .map_err(crate::lost::message)?,
            );
        }
        let mut subscriptions = Vec::new();
        for (subscription, request) in &started {
            connection
                .finish_subscribe(subscription, request)
                .map_err(crate::lost::message)?;
            subscriptions.push(subscription.clone());
        }
        // The session's name and run health, as they change (the Rust-only operations service).
        let ops = connection
            .subscribe(session, protocol::names::SESSION_OPS)
            .map_err(crate::lost::message)?;
        let ops_events = self.events.clone();
        connection.listen(
            &ops,
            Box::new(move |member, value| match member {
                "name" => ops_events(json!({"type": "session_name", "name": value["name"]})),
                "health" if value["stuck"] == true => ops_events(json!({"type": "run_stuck", "health": value})),
                _ => {}
            }),
        );
        subscriptions.push(ops);
        let transcript = started[2].0.clone();
        let events = self.events.clone();
        connection.listen(
            &transcript,
            Box::new(move |_, value| {
                if !value["event"].is_null() {
                    events(value["event"].clone());
                }
            }),
        );
        let mut state = self.lock();
        state.subscriptions = subscriptions;
        state.transcript = Some(transcript);
        state.session_id = id.to_string();
        state.attachment_id = attachment;
        Ok(())
    }

    /// Waits (bounded by [`SUPERVISE`]) for the link's own state to change: a rejoin starting or ending, closing.
    fn wait_link_change(&self) {
        let state = self.lock();
        drop(
            self.changed
                .wait_timeout(state, SUPERVISE)
                .unwrap_or_else(|e| e.into_inner()),
        );
    }

    /// What the supervisor waits on: the connection, or the link's own state while the connection is closed or a
    /// Radius recovery runs.
    fn wait_supervised_change(&self) {
        let connection = self.connection();
        if connection.is_closed() || self.radius_recovering.load(Ordering::SeqCst) {
            self.wait_link_change();
        } else {
            connection.wait_attachment_change(connection.attachment().as_ref(), SUPERVISE);
        }
    }

    fn set_reconnecting(&self, reconnecting: bool) {
        self.lock().reconnecting = reconnecting;
        self.changed.notify_all();
    }

    /// Wait (bounded) while a reconnection is under way or about to start (the supervisor has yet to notice a
    /// lost server or a moved attachment).
    fn wait_reconnected(&self) {
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        loop {
            let reconnecting = self.lock().reconnecting;
            if self.closing.load(Ordering::SeqCst)
                || self.lost_reported.load(Ordering::SeqCst)
                || Instant::now() >= deadline
                || (!reconnecting && self.needs_rejoin().is_none())
            {
                return;
            }
            let state = self.lock();
            drop(
                self.changed
                    .wait_timeout(state, SUPERVISE)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    /// What the supervisor sees: whether the link lost its server, or its attachment moved or ended.
    fn needs_rejoin(&self) -> Option<&'static str> {
        let state = self.lock();
        if state.session_id.is_empty() || state.attachment_id.is_none() && !state.reconnecting {
            return None;
        }
        if state.connection.is_closed() {
            return Some("The Ultron server went away");
        }
        let current = state.connection.attachment();
        let moved = current.as_ref().map(|a| (&a.session_id, &a.attachment_id))
            != Some((&state.session_id, state.attachment_id.as_ref()?));
        moved.then_some(WORKER_RESTARTED)
    }

    /// Rejoin the session: a new server generation (and connection) if the server is gone, then attach and
    /// subscribe again. Retries until it works or [`REJOIN_TIMEOUT`] passes.
    fn rejoin(&self, reason: &str) -> Result<(), String> {
        self.set_reconnecting(true);
        (self.events)(connection_event("reconnecting", Some(reason)));
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        let mut delay = Duration::from_millis(100);
        let result = loop {
            if self.closing.load(Ordering::SeqCst) {
                break Err("The link is closing".to_string());
            }
            match self.try_rejoin() {
                Ok(()) => break Ok(()),
                Err(error) if Instant::now() >= deadline => break Err(error),
                Err(_) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        };
        self.set_reconnecting(false);
        match &result {
            Ok(()) => (self.events)(connection_event("rejoined", None)),
            Err(error) => (self.events)(connection_event(
                "lost",
                Some(&crate::lost::lost_server_message(Some(error))),
            )),
        }
        result
    }

    fn try_rejoin(&self) -> Result<(), String> {
        let (closed, id) = {
            let state = self.lock();
            (state.connection.is_closed(), state.session_id.clone())
        };
        if closed {
            let Origin::Started { start, .. } = &self.origin else {
                return Err("The server connection closed".into());
            };
            let started = start()?;
            let connection =
                Connection::connect(&started.route.path, &started.route.server_id).map_err(crate::lost::message)?;
            let old = self.replace_connection(connection, started.stop);
            // The old generation already closed when it lost its coordinator; this only waits for that.
            if let Some(old) = old {
                std::thread::spawn(move || old());
            }
        }
        self.attach(&id)
    }

    /// Use `connection` from now on (and `server`, when this process started a new generation): the old
    /// connection's subscriptions are gone with it. Returns the replaced generation's stop.
    fn replace_connection(&self, connection: Arc<Connection>, server: Option<Stop>) -> Option<Stop> {
        let mut state = self.lock();
        state.connection = connection;
        state.subscriptions.clear();
        state.transcript = None;
        state.attachment_id = None;
        match server {
            Some(server) => state.server.replace(server),
            None => None,
        }
    }

    fn radius_route(&self) -> Option<&RemoteRoute> {
        match &self.origin {
            Origin::Remote(route @ RemoteRoute::Radius { .. }) => Some(route),
            _ => None,
        }
    }

    /// The Radius connection closed: report it and let the reconnector bring it back (`RadiusClientReconnect`:
    /// after 1 s, doubling to 30 s per failure), then re-attach the session. Operations wait meanwhile.
    fn recover_radius(self: &Arc<Self>) {
        if self.radius_recovering.swap(true, Ordering::SeqCst) {
            return;
        }
        self.set_reconnecting(true);
        (self.events)(radius_connection_event(ConnectionState::Disconnected));
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(reconnector) = reconnector {
            reconnector.connection_state_changed(ConnectionState::Disconnected);
        }
    }

    fn start_radius_reconnector(self: &Arc<Self>) {
        if self.radius_route().is_none() {
            return;
        }
        let client: Arc<dyn ReconnectClient> = Arc::new(RadiusLink(Arc::downgrade(self)));
        let shared = Arc::downgrade(self);
        let reattach: crate::radius::Reattach = Arc::new(move |session_id: &str| {
            let shared = shared.upgrade().ok_or("The link is closed")?;
            (shared.events)(radius_attachment_event("attaching", session_id));
            shared.attach(session_id)?;
            shared.radius_recovering.store(false, Ordering::SeqCst);
            shared.set_reconnecting(false);
            (shared.events)(radius_attachment_event("attached", session_id));
            Ok(())
        });
        let session = Some(self.lock().session_id.clone()).filter(|id| !id.is_empty());
        let reconnector = RadiusClientReconnect::new(client, session, reattach, None);
        *self.radius.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(reconnector));
    }

    /// Stop a Radius route's reconnector (`dispose()`).
    fn stop_radius_reconnector(&self) {
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(reconnector) = reconnector {
            reconnector.dispose();
        }
    }
}

/// The link as the Radius reconnector's client (`RadiusReconnectClient`): reconnecting opens a new relay connection
/// for the link; the connection's states are reported to the link's watcher.
struct RadiusLink(std::sync::Weak<Shared>);

impl ReconnectClient for RadiusLink {
    fn connected(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|shared| !shared.radius_connecting.load(Ordering::SeqCst) && !shared.connection().is_closed())
    }

    fn connection_state(&self) -> ConnectionState {
        match self.0.upgrade() {
            Some(shared) if shared.radius_connecting.load(Ordering::SeqCst) => ConnectionState::Connecting,
            Some(shared) if !shared.connection().is_closed() => ConnectionState::Connected,
            _ => ConnectionState::Disconnected,
        }
    }

    fn disconnect(&self, _reason: &str) {
        if let Some(shared) = self.0.upgrade() {
            shared.connection().disconnect();
            if !shared.closing.load(Ordering::SeqCst) {
                (shared.events)(radius_connection_event(ConnectionState::Disconnected));
            }
        }
    }

    fn reconnect(&self) -> Result<(), String> {
        let shared = self.0.upgrade().ok_or("The link is closed")?;
        let route = shared.radius_route().cloned().ok_or("Not a Radius route")?;
        shared.radius_connecting.store(true, Ordering::SeqCst);
        (shared.events)(radius_connection_event(ConnectionState::Connecting));
        let connected = route.connect();
        shared.radius_connecting.store(false, Ordering::SeqCst);
        match connected {
            Ok(connection) => {
                shared.replace_connection(connection, None);
                (shared.events)(radius_connection_event(ConnectionState::Connected));
                Ok(())
            }
            Err(error) => {
                (shared.events)(radius_connection_event(ConnectionState::Disconnected));
                Err(crate::lost::message(error))
            }
        }
    }
}

impl SessionLink {
    /// Connect to the server `start` brought up (`started`; `start` brings it up again when it goes away); every
    /// attach carries `session_options`. Transcript events go to `events`. Nothing is attached yet ([`Self::attach`],
    /// then [`Self::supervise`]); on failure the server is stopped.
    pub fn connect(
        start: Start,
        started: Started,
        session_options: Option<Value>,
        events: WatchSink,
    ) -> Result<SessionLink, String> {
        let connection = match Connection::connect(&started.route.path, &started.route.server_id) {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(stop) = started.stop {
                    stop();
                }
                return Err(crate::lost::message(error));
            }
        };
        let origin = Origin::Started { start, session_options };
        Ok(SessionLink::new(started.stop, connection, origin, events))
    }

    fn new(server: Option<Stop>, connection: Arc<Connection>, origin: Origin, events: WatchSink) -> SessionLink {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                server,
                connection,
                session_id: String::new(),
                attachment_id: None,
                subscriptions: Vec::new(),
                transcript: None,
                reconnecting: false,
            }),
            changed: Condvar::new(),
            origin,
            events,
            closing: AtomicBool::new(false),
            attaching: Mutex::new(()),
            kept_op: Mutex::new(None),
            radius: Mutex::new(None),
            radius_recovering: AtomicBool::new(false),
            radius_connecting: AtomicBool::new(false),
            lost_reported: AtomicBool::new(false),
        });
        SessionLink {
            shared,
            created: Vec::new(),
        }
    }

    /// Connect to a server this client does not run (`--connect`), select `selection`'s session on it
    /// (`prepareClientSession`) and attach; transcript events (and a Radius route's recovery events) go to `events`.
    pub fn open_remote(
        route: RemoteRoute,
        selection: &RemoteSelection,
        events: WatchSink,
    ) -> Result<SessionLink, String> {
        let connection = route.connect().map_err(crate::lost::message)?;
        let radius = matches!(route, RemoteRoute::Radius { .. });
        let mut link = SessionLink::new(None, connection, Origin::Remote(route), events);
        let selected = link
            .shared
            .management("describe", json!([{}]))
            .map(|listed| listed.and_then(|l| l.as_array().cloned()).unwrap_or_default())
            .and_then(|sessions| select_remote_session(selection, &sessions, radius))
            .and_then(|selected| match selected {
                Selected::Existing { session_id, rename } => {
                    if let Some(name) = rename {
                        link.shared.management("rename", json!([session_id, name]))?;
                    }
                    Ok(session_id)
                }
                Selected::Create(options) => link.create(options),
            });
        if let Err(error) = selected.and_then(|id| link.attach(&id)) {
            link.close(false);
            return Err(error);
        }
        link.supervise();
        Ok(link)
    }

    /// The route of a remote link (`None` for this process's own server).
    pub fn remote_route(&self) -> Option<RemoteRoute> {
        match &self.shared.origin {
            Origin::Remote(route) => Some(route.clone()),
            Origin::Started { .. } => None,
        }
    }

    /// Once the first session is attached: watch the connection and the attachment, and rejoin when either goes
    /// away. A Radius route's lost connection is recovered by its reconnector; a remote Unix route's is reported once
    /// (it has no reconnect).
    pub fn supervise(&self) {
        self.shared.start_radius_reconnector();
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || {
            loop {
                let Some(shared) = shared.upgrade() else { return };
                shared.wait_supervised_change();
                if shared.closing.load(Ordering::SeqCst) {
                    return;
                }
                if shared.radius_recovering.load(Ordering::SeqCst) {
                    continue;
                }
                let Some(reason) = shared.needs_rejoin() else {
                    continue;
                };
                let closed = shared.connection().is_closed();
                match &shared.origin {
                    Origin::Remote(RemoteRoute::Radius { .. }) if closed => shared.recover_radius(),
                    Origin::Remote(RemoteRoute::Unix(_)) if closed => {
                        if !shared.lost_reported.swap(true, Ordering::SeqCst) {
                            (shared.events)(connection_event(
                                "lost",
                                Some(&crate::lost::lost_server_message(Some(reason))),
                            ));
                        }
                    }
                    _ => {
                        let _ = shared.rejoin(reason);
                    }
                }
            }
        });
    }

    /// The current connection (a new one after the server was restarted).
    pub fn connection(&self) -> Arc<Connection> {
        self.shared.connection()
    }

    /// A handle another thread can follow the link with (its current connection and session).
    pub fn handle(&self) -> LinkHandle {
        LinkHandle {
            shared: self.shared.clone(),
        }
    }

    pub fn session_id(&self) -> String {
        self.shared.lock().session_id.clone()
    }

    /// Create a session (`ultron.session-management.create` options); returns its id.
    pub fn create(&mut self, options: Value) -> Result<String, String> {
        let summary = self.shared.management("create", json!([options]))?.unwrap_or_default();
        let id = summary["sessionId"].as_str().unwrap_or_default().to_string();
        self.created.push(id.clone());
        Ok(id)
    }

    /// Remove a stored session through the server (`ultron.session-management.remove`).
    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.shared.management("remove", json!([id])).map(|_| ())
    }

    /// Import a Pi session's text through the server (`ultron.session-management.importPi`): `{session, alreadyImported,
    /// imported}`.
    pub fn import_pi(&self, source_path: &str, content: &str) -> Result<Value, String> {
        let source = json!({"sourcePath": source_path, "content": content});
        Ok(self.shared.management("importPi", json!([source]))?.unwrap_or_default())
    }

    /// Rename a stored session through the server (`ultron.session-management.rename`).
    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        self.shared.management("rename", json!([id, name])).map(|_| ())
    }

    /// The file of a session the server lists.
    pub fn session_file(&self, id: &str) -> Result<Option<String>, String> {
        let listed = self.shared.management("describe", json!([{}]))?.unwrap_or_default();
        Ok(listed
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["sessionId"] == id)
            .and_then(|s| s["sessionFile"].as_str().map(str::to_string)))
    }

    /// Attach to session `id` (replacing the current one): its services subscribed, its transcript followed.
    pub fn attach(&mut self, id: &str) -> Result<(), String> {
        self.shared.wait_reconnected();
        self.shared.attach(id)
    }

    /// The attached session's transcript snapshot (`LaneSnapshot`).
    pub fn snapshot(&self) -> Value {
        let state = self.shared.lock();
        state
            .transcript
            .as_ref()
            .and_then(|t| state.connection.state(t, "state"))
            .map_or(Value::Null, |s| s["snapshot"].clone())
    }

    /// Run a session operation (`session_core::core::Op` as JSON) on the attached session's worker.
    pub fn op(&self, op: impl Into<Value>) -> CallResult {
        self.call(protocol::names::SESSION_OPS, "op", json!([op.into()]))
    }

    /// Run `op` on the attached session now, and again on every session attached later (which starts without it):
    /// the terminal's appearance the TUI detected.
    pub fn keep_op(&self, op: impl Into<Value>) -> CallResult {
        let op = op.into();
        *self.shared.kept_op.lock().unwrap_or_else(|e| e.into_inner()) = Some(op.clone());
        self.op(op)
    }

    /// A blocking call of the attached session (`bash`, `compact`, the agent controller's `prompt`), after any
    /// reconnection under way; a call refused because the worker was restarting never reached it and is sent again
    /// once the link rejoined.
    pub fn call(&self, service: &str, member: &str, args: Value) -> CallResult {
        self.shared.session_call(service, member, args)
    }

    /// Detach, remove the sessions this link created when `remove_created`, and stop the server.
    pub fn close(mut self, remove_created: bool) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        // A recovery under way is abandoned (the reconnector stops and disconnects).
        if self.shared.radius_recovering.load(Ordering::SeqCst) {
            self.shared.stop_radius_reconnector();
        }
        {
            let mut state = self.shared.lock();
            self.shared.unsubscribe(&mut state);
        }
        let connection = self.shared.connection();
        if remove_created {
            for id in std::mem::take(&mut self.created) {
                let attached = connection.attachment().is_some_and(|a| a.session_id == id);
                if self.shared.management("remove", json!([id])).is_ok() && attached {
                    let _ = connection.wait_attachment(|a| a.is_none());
                }
            }
        }
        self.shared.stop_radius_reconnector();
        connection.disconnect();
        let server = self.shared.lock().server.clone();
        if let Some(stop) = server {
            stop();
        }
    }
}
