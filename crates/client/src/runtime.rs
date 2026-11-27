//! The client runtime print, json and rpc modes (and later the TUI) use: a pipelined connection
//! (packages/client client.ts) whose service subscriptions keep replicas of the server's replicated states
//! (Chord's consumer side), and `run_client` (experimental/client.ts `runClient`): list sessions, attach to one, or
//! create one and prompt it, following its transcript until the run ends.

mod runner;

pub use runner::{
    ClientCommand, ClientResult, EventListener, REJOIN_TIMEOUT, Reopen, RunOptions, rejoin, run_client, run_client_on,
    run_client_reopening, run_client_with,
};

use chord_delta::{Decoder, Op};
use protocol::json::{to_cbor, to_json};
use protocol::{
    ClientMessage, DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerMessage,
    ServerMessageDecoder, SessionTarget, encode_client_message, is_server_id,
};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A failed call: a bounded server error, or a lost connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Server(ProtocolError),
    Disconnected(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Server(error) => f.write_str(&error.message),
            CallError::Disconnected(message) => f.write_str(message),
        }
    }
}

/// Receives `(member, value)` after each applied state update of one subscription.
pub type UpdateListener = Box<dyn Fn(&str, &Json) + Send>;

/// One state member of a singleton replica (`MemberSlot` over `ReplicatedStateReplica`): `None` until hydrated and
/// after it was cleared (`unavailable`, a sequence gap, an update that does not apply).
struct Slot {
    value: Option<Json>,
    sequence: u64,
}

/// A failure while applying a service update: `Fatal` breaks the operation stream (the wire decoder, `client.ts`
/// fails the connection), `Reported` is the consumer's `reportError` (state.ts/consumer.ts): only that replica's
/// member is cleared and the connection stays up.
enum UpdateError {
    Fatal(String),
    Reported(String),
}

/// The client's replica of one singleton subscription: the wire decoders (`createServiceStateDecoder`) and the
/// consumer facade (`ServiceFacade` of `RemoteServiceBinding`, consumer.ts:579-608).
struct Replica {
    service: String,
    decoders: HashMap<String, Decoder>,
    slots: HashMap<String, Slot>,
    listener: Option<UpdateListener>,
    /// Errors the consumer reported (`onError`), oldest first.
    errors: Vec<String>,
}

fn is_base(ops: &[Op]) -> bool {
    matches!(ops.first(), Some(Op::Replace(_)))
}

fn wire_ops(member: &Json) -> &[Json] {
    member["ops"].as_array().map_or_default(Vec::as_slice)
}

impl Replica {
    /// Decode one instance snapshot's state members with fresh decoders (`decodeInstance`).
    fn decode_instance(&mut self, instance: &Json) -> Result<Vec<(String, u64, Vec<Op>)>, String> {
        let mut states = Vec::new();
        for member in instance["members"].as_array().into_iter().flatten() {
            if member["kind"] != "state" {
                continue;
            }
            let name = member["name"].as_str().unwrap_or_default().to_string();
            if self.decoders.contains_key(&name) {
                return Err(format!("Duplicate service state {name}"));
            }
            let decoder = self.decoders.entry(name.clone()).or_default();
            let ops = decoder.decode(wire_ops(member)).map_err(|e| e.to_string())?;
            states.push((name, member["sequence"].as_u64().unwrap_or_default(), ops));
        }
        Ok(states)
    }

    /// `validateMembers`: names are non-empty and unique.
    fn validate_members(instance: &Json) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        for member in instance["members"].as_array().into_iter().flatten() {
            let name = member["name"].as_str().unwrap_or_default();
            if name.is_empty() || !names.insert(name) {
                return Err("Remote service has invalid member descriptions".into());
            }
        }
        Ok(())
    }

    /// `ServiceFacade.install`: hydrate every state member from a base batch; a failing member is cleared.
    fn install(&mut self, instance: &Json, states: Vec<(String, u64, Vec<Op>)>) -> Result<(), String> {
        Replica::validate_members(instance)?;
        let names: Vec<&str> = instance["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str())
            .collect();
        let mut existing: Vec<&String> = self.slots.keys().collect();
        existing.sort();
        if let Some(missing) = existing.into_iter().find(|name| !names.contains(&name.as_str())) {
            return Err(format!("Unknown remote service member {}.{missing}", self.service));
        }
        let mut states: HashMap<String, (u64, Vec<Op>)> = states
            .into_iter()
            .map(|(name, sequence, ops)| (name, (sequence, ops)))
            .collect();
        for member in instance["members"].as_array().into_iter().flatten() {
            let name = member["name"].as_str().unwrap_or_default().to_string();
            let Some((sequence, ops)) = states.remove(&name) else {
                if self.slots.contains_key(&name) {
                    return Err(format!("Remote service member {}.{name} changed kind", self.service));
                }
                continue;
            };
            let slot = self.slots.entry(name.clone()).or_insert(Slot {
                value: None,
                sequence: 0,
            });
            if !is_base(&ops) {
                slot.value = None;
                return Err("Replicated state snapshot is not a base operation batch".into());
            }
            match chord_delta::apply(Json::Null, ops) {
                Ok(value) => {
                    slot.sequence = sequence;
                    slot.value = Some(value);
                }
                Err(error) => {
                    slot.value = None;
                    return Err(error.to_string());
                }
            }
            if let (Some(listener), Some(value)) = (&self.listener, &slot.value) {
                listener(&name, value);
            }
        }
        Ok(())
    }

    /// The subscribe response: one singleton instance (`#startSingleton`).
    fn hydrate(service: &str, snapshot: &Json) -> Result<Replica, String> {
        let mut replica = Replica {
            service: service.to_string(),
            decoders: HashMap::new(),
            slots: HashMap::new(),
            listener: None,
            errors: Vec::new(),
        };
        let instances = snapshot["instances"].as_array().map_or_default(Vec::as_slice);
        let mut decoded = Vec::new();
        for instance in instances {
            decoded.push(replica.decode_instance(instance)?);
        }
        if snapshot["mode"] != "singleton" || snapshot["serviceId"] != service || instances.len() != 1 {
            return Err(format!(
                "Remote service {service} returned an invalid singleton snapshot"
            ));
        }
        let states = decoded.pop().unwrap_or_default();
        replica.install(&instances[0], states)?;
        Ok(replica)
    }

    fn update(&mut self, update: &Json) -> Result<(), UpdateError> {
        match update["type"].as_str() {
            Some("state") => self.apply_state_update(update),
            Some("replaced") => self.apply_replaced(update),
            Some("unavailable") => {
                self.decoders.clear();
                for slot in self.slots.values_mut() {
                    slot.value = None;
                }
                Ok(())
            }
            Some("spawned" | "closed") => Ok(()),
            _ => Err(UpdateError::Fatal("Invalid service provider update".into())),
        }
    }

    fn state_decoder(&mut self, member: &str, instance: &Json) -> Result<&mut Decoder, UpdateError> {
        if instance.is_null() {
            return self
                .decoders
                .get_mut(member)
                .ok_or_else(|| UpdateError::Fatal(format!("Unknown service state {member}")));
        }
        let name = format!(
            "{}@{}.{member}",
            jsrt::value::string(Some(&instance["key"])),
            jsrt::value::string(Some(&instance["generation"]))
        );
        Err(UpdateError::Fatal(format!("Unknown service state {name}")))
    }

    fn apply_state_update(&mut self, update: &Json) -> Result<(), UpdateError> {
        let member = update["member"].as_str().unwrap_or_default();
        let decoder = self.state_decoder(member, &update["instance"])?;
        let ops = decoder
            .decode(wire_ops(update))
            .map_err(|e| UpdateError::Fatal(e.to_string()))?;
        let sequence = update["sequence"].as_u64().unwrap_or_default();
        let Some(slot) = self.slots.get_mut(member) else {
            return Err(UpdateError::Reported(format!(
                "Remote service update targets non-state member {}.{member}",
                self.service
            )));
        };
        let Some(value) = slot.value.take() else {
            return Err(UpdateError::Reported(
                "Replicated state received an update before hydration".into(),
            ));
        };
        if sequence != slot.sequence + 1 {
            return Err(UpdateError::Reported(
                "Replicated state update sequence has a gap".into(),
            ));
        }
        let next = chord_delta::apply(value, ops).map_err(|e| UpdateError::Reported(e.to_string()))?;
        slot.sequence = sequence;
        if let Some(listener) = &self.listener {
            listener(member, &next);
        }
        slot.value = Some(next);
        Ok(())
    }

    fn apply_replaced(&mut self, update: &Json) -> Result<(), UpdateError> {
        self.decoders.clear();
        let snapshot = &update["snapshot"];
        let states = self.decode_instance(snapshot).map_err(UpdateError::Fatal)?;
        if !snapshot["instance"].is_null() {
            return Err(UpdateError::Reported(
                "Singleton replacement has an instance address".into(),
            ));
        }
        self.install(snapshot, states).map_err(UpdateError::Reported)
    }

    /// Apply one update; a reported error stays with this replica, a fatal one is returned.
    fn receive(&mut self, update: &Json) -> Result<(), String> {
        match self.update(update) {
            Ok(()) => Ok(()),
            Err(UpdateError::Reported(error)) => {
                self.errors.push(error);
                Ok(())
            }
            Err(UpdateError::Fatal(error)) => Err(error),
        }
    }
}

#[derive(Default)]
struct Shared {
    responses: HashMap<String, Result<Option<Json>, ProtocolError>>,
    replicas: HashMap<String, Replica>,
    /// Updates of a subscription whose snapshot has not been read yet.
    early: HashMap<String, Vec<Json>>,
    /// The service each pending subscription subscribes to.
    services: HashMap<String, String>,
    attachment: Option<SessionTarget>,
    closed: Option<String>,
}

/// The write half of a connection's byte transport: a Unix socket, or the Radius relay
/// ([`crate::radius::RadiusByteWriter`]).
pub trait ConnectionWriter: Write + Send {
    /// End the transport (both directions); the reader then sees it closed.
    fn shutdown(&mut self);
}

impl ConnectionWriter for UnixStream {
    fn shutdown(&mut self) {
        let _ = UnixStream::shutdown(self, std::net::Shutdown::Both);
    }
}

/// A handshaken connection to one server; requests are pipelined and answered by a reader thread.
pub struct Connection {
    pub server_id: String,
    writer: Mutex<Box<dyn ConnectionWriter>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    requests: AtomicU64,
    subscriptions: AtomicU64,
}

impl Connection {
    pub fn connect(path: &std::path::Path, server_id: &str) -> Result<Arc<Connection>, CallError> {
        let disconnected = |e: std::io::Error| CallError::Disconnected(e.to_string());
        if !is_server_id(server_id) {
            return Err(CallError::Disconnected(
                "serverId must be a canonical lowercase UUIDv4".into(),
            ));
        }
        let stream = UnixStream::connect(path).map_err(disconnected)?;
        let read = stream.try_clone().map_err(disconnected)?;
        Connection::connect_over(Box::new(read), Box::new(stream), server_id)
    }

    /// The server `server_id` through the Radius gateway (`createRadiusClientTransportFactory`): `auth` is resolved
    /// for this connection (required).
    pub fn connect_radius(
        server_id: &str,
        auth: &dyn server::radius_relay::RelayAuthResolver,
    ) -> Result<Arc<Connection>, CallError> {
        if !is_server_id(server_id) {
            return Err(CallError::Disconnected(
                "serverId must be a canonical lowercase UUIDv4".into(),
            ));
        }
        let (read, write) = crate::radius::RadiusByteStream::connect(server_id, auth)
            .map_err(CallError::Disconnected)?
            .split();
        Connection::connect_over(Box::new(read), Box::new(write), server_id)
    }

    pub fn connect_over(
        mut read: Box<dyn Read + Send>,
        writer: Box<dyn ConnectionWriter>,
        server_id: &str,
    ) -> Result<Arc<Connection>, CallError> {
        let connection = Arc::new(Connection {
            server_id: server_id.to_string(),
            writer: Mutex::new(writer),
            shared: Arc::new((Mutex::new(Shared::default()), Condvar::new())),
            requests: AtomicU64::new(0),
            subscriptions: AtomicU64::new(0),
        });
        let protocol::handshake::Connected { decoder, queued } = connection.handshake(&mut read, server_id)?;
        let shared = connection.shared.clone();
        for message in queued {
            dispatch(&shared, message);
        }
        spawn_reader_thread(read, decoder, shared);
        Ok(connection)
    }

    fn handshake(&self, read: &mut dyn Read, server_id: &str) -> Result<protocol::handshake::Connected, CallError> {
        self.write(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })?;
        match protocol::handshake::read_hello(read, server_id) {
            Ok(connected) => Ok(connected),
            Err(error) => {
                self.disconnect();
                Err(match error {
                    protocol::handshake::HelloError::Rejected(error) => CallError::Server(error),
                    other => CallError::Disconnected(other.to_string()),
                })
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        jsrt::sync::lock(&self.shared.0)
    }

    fn write(&self, message: &ClientMessage) -> Result<(), CallError> {
        let frame =
            encode_client_message(message, DEFAULT_MAX_FRAME_LENGTH).map_err(|e| CallError::Disconnected(e.0))?;
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(&frame)
            .map_err(|e| CallError::Disconnected(e.to_string()))
    }

    pub fn attachment(&self) -> Option<SessionTarget> {
        self.lock().attachment.clone()
    }

    pub fn server_target(&self) -> RpcTarget {
        RpcTarget::Server {
            server_id: self.server_id.clone(),
        }
    }

    /// `plugins.prepareSession` for session `id` with the default packages: the plugin selection `/reload` rebuilds.
    pub fn prepare_session(&self, id: &str) -> Result<Option<Json>, CallError> {
        self.call(
            self.server_target(),
            protocol::names::PRESENTATION_PLUGINS,
            "prepareSession",
            json!([{"sessionId": id, "packagePaths": null}]),
        )
    }

    pub fn session_target(&self) -> Result<RpcTarget, CallError> {
        self.attachment()
            .map(RpcTarget::Session)
            .ok_or_else(|| CallError::Disconnected("Session is not attached".into()))
    }

    /// Send one call; its response is awaited with [`Connection::wait`].
    pub fn start(&self, target: RpcTarget, service: &str, member: &str, args: Json) -> Result<String, CallError> {
        let id = format!("request-{}", self.requests.fetch_add(1, Ordering::SeqCst) + 1);
        let call = json!({"serviceId": service, "member": member, "args": args});
        self.write(&ClientMessage::Request {
            id: id.clone(),
            target,
            call: to_cbor(&call),
        })?;
        Ok(id)
    }

    /// Cancel request `id` (sent to `target`): the server aborts it if it is still active, and its response is then
    /// the `cancelled` error (client.ts sends `{type: "cancel", id, target}` when a call's signal aborts).
    pub fn cancel(&self, target: RpcTarget, id: &str) -> Result<(), CallError> {
        self.write(&ClientMessage::Cancel {
            id: id.to_string(),
            target,
        })
    }

    /// Send several calls in one write (as a pipelining client does in one tick); returns their request ids.
    pub fn start_all(&self, calls: Vec<(RpcTarget, &str, &str, Json)>) -> Result<Vec<String>, CallError> {
        let mut frames = Vec::new();
        let mut ids = Vec::new();
        for (target, service, member, args) in calls {
            let id = format!("request-{}", self.requests.fetch_add(1, Ordering::SeqCst) + 1);
            let call = json!({"serviceId": service, "member": member, "args": args});
            let message = ClientMessage::Request {
                id: id.clone(),
                target,
                call: to_cbor(&call),
            };
            frames.extend(
                encode_client_message(&message, DEFAULT_MAX_FRAME_LENGTH).map_err(|e| CallError::Disconnected(e.0))?,
            );
            ids.push(id);
        }
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .write_all(&frames)
            .map_err(|e| CallError::Disconnected(e.to_string()))?;
        Ok(ids)
    }

    pub fn wait(&self, id: &str) -> Result<Option<Json>, CallError> {
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(result) = shared.responses.remove(id) {
                return result.map_err(CallError::Server);
            }
            if let Some(reason) = &shared.closed {
                return Err(CallError::Disconnected(reason.clone()));
            }
            shared = changed.wait(shared).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// [`Connection::wait`] for at most `timeout`; `None` when the response has not arrived.
    pub fn wait_for(&self, id: &str, timeout: Duration) -> Option<Result<Option<Json>, CallError>> {
        let deadline = Instant::now() + timeout;
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(result) = shared.responses.remove(id) {
                return Some(result.map_err(CallError::Server));
            }
            if let Some(reason) = &shared.closed {
                return Some(Err(CallError::Disconnected(reason.clone())));
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            shared = changed
                .wait_timeout(shared, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    pub fn call(&self, target: RpcTarget, service: &str, member: &str, args: Json) -> Result<Option<Json>, CallError> {
        let id = self.start(target, service, member, args)?;
        self.wait(&id)
    }

    /// Start a subscription; returns `(subscription id, request id)`.
    pub fn start_subscribe(&self, target: RpcTarget, service: &str) -> Result<(String, String), CallError> {
        let subscription = format!("service-{}", self.subscriptions.fetch_add(1, Ordering::SeqCst) + 1);
        let mut shared = self.lock();
        shared.early.insert(subscription.clone(), Vec::new());
        shared.services.insert(subscription.clone(), service.to_string());
        drop(shared);
        let request = self.start(
            target,
            protocol::names::CHORD_SERVICE,
            "subscribe",
            json!([subscription, service, "singleton"]),
        )?;
        Ok((subscription, request))
    }

    /// Finish a subscription: hydrate its replica from the snapshot and apply updates that came before it.
    pub fn finish_subscribe(&self, subscription: &str, request: &str) -> Result<(), CallError> {
        let snapshot = self.wait(request)?.unwrap_or(Json::Null);
        let service = self.lock().services.remove(subscription).unwrap_or_default();
        let mut replica = Replica::hydrate(&service, &snapshot).map_err(CallError::Disconnected)?;
        let mut shared = self.lock();
        for update in shared.early.remove(subscription).unwrap_or_default() {
            replica.receive(&update).map_err(CallError::Disconnected)?;
        }
        shared.replicas.insert(subscription.to_string(), replica);
        Ok(())
    }

    pub fn subscribe(&self, target: RpcTarget, service: &str) -> Result<String, CallError> {
        let (subscription, request) = self.start_subscribe(target, service)?;
        self.finish_subscribe(&subscription, &request)?;
        Ok(subscription)
    }

    pub fn start_unsubscribe(&self, target: RpcTarget, subscription: &str) -> Result<String, CallError> {
        self.start(
            target,
            protocol::names::CHORD_SERVICE,
            "unsubscribe",
            json!([subscription]),
        )
    }

    pub fn forget(&self, subscription: &str) {
        let mut shared = self.lock();
        shared.replicas.remove(subscription);
        shared.early.remove(subscription);
        shared.services.remove(subscription);
    }

    pub fn state(&self, subscription: &str, member: &str) -> Option<Json> {
        self.lock()
            .replicas
            .get(subscription)
            .and_then(|r| r.slots.get(member))
            .and_then(|slot| slot.value.clone())
    }

    /// Take the errors the subscription's consumer reported (`onError`): a sequence gap, an update before hydration,
    /// a non-base snapshot, an update that does not apply. Each cleared only its own member.
    pub fn take_replica_errors(&self, subscription: &str) -> Vec<String> {
        self.lock()
            .replicas
            .get_mut(subscription)
            .map_or_default(|r| std::mem::take(&mut r.errors))
    }

    pub fn listen(&self, subscription: &str, listener: UpdateListener) {
        if let Some(replica) = self.lock().replicas.get_mut(subscription) {
            replica.listener = Some(listener);
        }
    }

    pub fn unlisten(&self, subscription: &str) {
        if let Some(replica) = self.lock().replicas.get_mut(subscription) {
            replica.listener = None;
        }
    }

    pub fn wait_attachment(&self, done: impl Fn(Option<&SessionTarget>) -> bool) -> Result<(), CallError> {
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if done(shared.attachment.as_ref()) {
                return Ok(());
            }
            if let Some(reason) = &shared.closed {
                return Err(CallError::Disconnected(reason.clone()));
            }
            shared = changed.wait(shared).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    /// Blocks until the connection closed or its attachment is no longer `seen`, or `timeout` passed.
    pub fn wait_attachment_change(&self, seen: Option<&SessionTarget>, timeout: Duration) {
        let (lock, changed) = &*self.shared;
        let shared = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            changed
                .wait_timeout_while(shared, timeout, |shared| {
                    shared.closed.is_none() && shared.attachment.as_ref() == seen
                })
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    pub fn disconnect(&self) {
        self.writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .shutdown();
    }
}

fn dispatch(shared: &Arc<(Mutex<Shared>, Condvar)>, message: ServerMessage) {
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.closed.is_some() {
        return;
    }
    match message {
        ServerMessage::Response { id, result } => {
            state.responses.insert(id, result.map(|r| r.as_ref().map(to_json)));
        }
        ServerMessage::Attachment { attachment } => state.attachment = attachment,
        ServerMessage::ServiceUpdate {
            subscription_id,
            update,
        } => {
            let update = to_json(&update);
            if let Some(early) = state.early.get_mut(&subscription_id) {
                early.push(update);
            } else if let Some(replica) = state.replicas.get_mut(&subscription_id)
                && let Err(error) = replica.receive(&update)
            {
                state.closed = Some(error);
            }
        }
        ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
            state.closed = Some("Unexpected handshake message".into());
        }
    }
    drop(state);
    changed.notify_all();
}

fn spawn_reader_thread(
    mut read: Box<dyn Read + Send>,
    mut decoder: ServerMessageDecoder,
    shared: Arc<(Mutex<Shared>, Condvar)>,
) {
    std::thread::spawn(move || {
        let reason = read_until_closed(&mut *read, &mut decoder, &shared);
        let (lock, changed) = &*shared;
        jsrt::sync::lock(lock).closed.get_or_insert(reason);
        changed.notify_all();
    });
}

fn read_until_closed(
    read: &mut dyn Read,
    decoder: &mut ServerMessageDecoder,
    shared: &Arc<(Mutex<Shared>, Condvar)>,
) -> String {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let n = match read.read(&mut buffer) {
            Ok(0) => return "Byte transport closed".to_string(),
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return e.to_string(),
        };
        match decoder.push(&buffer[..n]) {
            Ok(messages) => {
                for message in messages {
                    dispatch(shared, message);
                }
            }
            Err(error) => return error.0,
        }
    }
}
