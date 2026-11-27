//! `SessionWorkerManager` (experimental/session-worker-manager.ts): the session workers one server generation owns.
//! It launches a worker per session, adopts orphans through discovery, applies attachment demand, routes service
//! operations and their updates through the coordinator, and stops, releases or forgets workers.

mod events;
mod types;

pub use types::{
    AttachmentDemand, CANCELLED_CODE, CANCELLED_MESSAGE, DemandError, DemandLedger, HEARTBEAT_ENV, MAX_RESTARTS,
    RESTART_BASE, RESTART_MAX_DELAY, RESTART_STABLE, Terminated, WorkerModel, restart_delay,
};

use crate::catalog::Meta;
use crate::chord::{Publish, ServiceError, ServiceResult, subscribe_id, unsubscribe_id};
use crate::coord_client::CoordinatorConnection;
use crate::process::{self, Role};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use types::{heartbeat_period, startup_timeout};

const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(10_000);
const WORKER_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(5_000);
const WORKER_DEMAND_TIMEOUT: Duration = Duration::from_millis(5_000);
const HEARTBEAT_MISSES: u32 = 6;
/// What a retiring worker answers a demand (crate::lifecycle `set_demand`).
const RETIRING_REJECTION: &str = "Session worker is retiring";

struct Worker {
    peer: String,
    meta: Meta,
    pid: u32,
    token: String,
    demand: DemandLedger,
    expected_stop: bool,
    retiring: bool,
    stopping: bool,
    terminated: Arc<Terminated>,
    /// A run is in progress (the worker's last status).
    busy: bool,
    /// The worker reported its run stuck (logged once per episode).
    stuck: bool,
    /// When the worker last answered (any message from it).
    last_seen: Instant,
    started: Instant,
}

struct Pending {
    peer: String,
    token: String,
    pid: u32,
    outcome: Option<Result<String, ServiceError>>,
    /// The process exited before readiness while the coordinator still listed it: its disconnect settles the
    /// launch with this error, unless a `worker_failed` relayed before the disconnect already did.
    exited: Option<ServiceError>,
}

/// A crashed worker's restart: attempts since it last ran stably, and whether one is under way.
struct Restart {
    attempts: u32,
    pending: bool,
}

type Done = Box<dyn FnOnce(ServiceResult) + Send>;

#[derive(Default)]
struct Inner {
    workers: HashMap<String, Worker>,
    by_session: HashMap<PathBuf, String>,
    pending: HashMap<PathBuf, Pending>,
    demands: HashMap<String, (String, Option<Result<(), String>>)>,
    operations: HashMap<String, (String, Value, Done)>,
    subscriptions: HashMap<String, (String, Publish)>,
    releases: HashMap<String, (String, Option<bool>, bool)>,
    foreign: HashMap<PathBuf, (String, u32)>,
    discovery: Option<HashSet<String>>,
    exited: HashMap<String, Worker>,
    restarts: HashMap<PathBuf, Restart>,
    detached: bool,
    shutting_down: bool,
    releasing: bool,
}

/// A routed session's worker, as the router holds it.
#[derive(Clone)]
pub struct Handle {
    peer: String,
    pub session_id: String,
    pub path: PathBuf,
    pub terminated: Arc<Terminated>,
}

type Logger = Box<dyn Fn(&str, &str) + Send + Sync>;
/// Told a worker's report of its session's active schedules (crate::prompt_schedules).
type SchedulesListener = Box<dyn Fn(&Meta, &Value) + Send + Sync>;

pub struct Manager {
    me: std::sync::Weak<Manager>,
    coordinator: OnceLock<Arc<CoordinatorConnection>>,
    session_dir: PathBuf,
    model: WorkerModel,
    /// The options a session's worker starts on when a client gave its own (`ultron durable --model ...`), by
    /// session path; kept for its restarts, replaced when a later client starts it again.
    session_models: Mutex<HashMap<PathBuf, WorkerModel>>,
    inner: Mutex<Inner>,
    changed: Condvar,
    on_count: Box<dyn Fn(usize) + Send + Sync>,
    logger: OnceLock<Logger>,
    schedules: OnceLock<SchedulesListener>,
    /// Settings defaults its workers apply under the user's files (`settingsDefaults`; the durable instance's
    /// `persona: "ultron"`).
    settings_defaults: OnceLock<Value>,
    /// Whether this is the durable instance's manager: its workers start new sessions on `durable.model` and save
    /// a chosen model there (`durable` worker option, config::settings `set_durable_process`).
    durable: OnceLock<()>,
}

fn scoped(scope: &Value, subscription: &str) -> String {
    format!(
        "{}\0{}\0{subscription}",
        scope["serverConnectionId"].as_str().unwrap_or_default(),
        scope["attachmentId"].as_str().unwrap_or_default()
    )
}

impl Manager {
    pub fn new(session_dir: PathBuf, model: WorkerModel, on_count: Box<dyn Fn(usize) + Send + Sync>) -> Arc<Manager> {
        Arc::new_cyclic(|me| Manager {
            me: me.clone(),
            coordinator: OnceLock::new(),
            session_dir,
            model,
            session_models: Mutex::new(HashMap::new()),
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            on_count,
            logger: OnceLock::new(),
            schedules: OnceLock::new(),
            settings_defaults: OnceLock::new(),
            durable: OnceLock::new(),
        })
    }

    /// Workers started from now on apply `defaults` under the user's settings files (config::settings
    /// `set_process_defaults`).
    pub fn set_settings_defaults(&self, defaults: Value) {
        let _ = self.settings_defaults.set(defaults);
    }

    /// Workers started from now on are the durable instance's (see `durable`).
    pub fn set_durable(&self) {
        let _ = self.durable.set(());
    }

    pub fn set_coordinator(&self, coordinator: Arc<CoordinatorConnection>) {
        let _ = self.coordinator.set(coordinator);
        self.start_heartbeat();
    }

    /// Where restarts, hung workers and stuck runs are reported (the server log).
    pub fn set_logger(&self, logger: Logger) {
        let _ = self.logger.set(logger);
    }

    /// Where workers' reports of their sessions' active schedules go (the server's schedule timer).
    pub fn on_schedules(&self, listener: SchedulesListener) {
        let _ = self.schedules.set(listener);
    }

    fn log(&self, label: &str, message: &str) {
        if let Some(logger) = self.logger.get() {
            logger(label, message);
        }
    }

    /// Ping every worker each period; one silent for [`HEARTBEAT_MISSES`] periods is hung: killed, and restarted
    /// like any crashed worker that had work.
    fn start_heartbeat(&self) {
        let period = heartbeat_period();
        let this = self.me.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(period);
                let Some(this) = this.upgrade() else { return };
                type Owed = Vec<(String, Vec<String>)>;
                let (live, hung, owed): (Vec<String>, Vec<(u32, String, Duration)>, Owed) = {
                    let inner = this.lock();
                    if inner.detached || inner.shutting_down {
                        return;
                    }
                    let active = inner.workers.values().filter(|w| !w.stopping && !w.expected_stop);
                    let (hung, live): (Vec<&Worker>, Vec<&Worker>) =
                        active.partition(|w| w.last_seen.elapsed() > period * HEARTBEAT_MISSES);
                    (
                        live.iter().map(|w| w.peer.clone()).collect(),
                        hung.iter()
                            .map(|w| (w.pid, w.meta.id.clone(), w.last_seen.elapsed()))
                            .collect(),
                        live.iter()
                            .map(|w| (w.peer.clone(), w.demand.owed()))
                            .filter(|(_, owed)| !owed.is_empty())
                            .collect(),
                    )
                };
                for (pid, id, silent) in hung {
                    this.log(
                        "[ultron server warning]",
                        &format!(
                            "Session worker {id} (pid {pid}) did not answer heartbeats for {} ms; killing it",
                            silent.as_millis()
                        ),
                    );
                    process::kill(pid, rustix::process::Signal::KILL);
                }
                for peer in live {
                    let _ = this.send(&peer, json!({"type": "ping", "nonce": jsrt::random::uuid_v4()}));
                }
                for (peer, attachments) in owed {
                    for attachment in attachments {
                        let message = this.demand_message(&jsrt::random::uuid_v4(), &attachment, false);
                        let _ = this.send(&peer, message);
                    }
                }
            }
        });
    }

    /// A restart of `path`'s crashed worker is under way.
    pub fn restart_pending(&self, path: &PathBuf) -> bool {
        self.lock().restarts.get(path).is_some_and(|r| r.pending)
    }

    /// Wait for `path`'s restart to finish; the restarted worker's handle, if it came back.
    pub fn wait_restart(&self, path: &PathBuf, timeout: Duration) -> Option<Handle> {
        let (inner, _) = self.wait_until(timeout, |i| {
            i.detached || i.shutting_down || !i.restarts.get(path).is_some_and(|r| r.pending)
        });
        let peer = inner.by_session.get(path)?;
        Some(self.handle(&inner.workers[peer]))
    }

    /// Whether a worker that ended unexpectedly is restarted: it had a run in progress or clients attached, and
    /// the server is not going away.
    fn should_restart(inner: &Inner, worker: &Worker) -> bool {
        !inner.detached
            && !inner.shutting_down
            && !inner.releasing
            && !worker.expected_stop
            && !worker.retiring
            && (worker.busy || worker.demand.any_attached())
    }

    fn run_restart_attempt(this: std::sync::Weak<Manager>, meta: Meta, attempt: u32) {
        std::thread::sleep(restart_delay(attempt));
        let Some(this) = this.upgrade() else { return };
        let launched = {
            let inner = this.lock();
            if inner.detached || inner.shutting_down || inner.releasing {
                None
            } else if inner.by_session.contains_key(&meta.path) || inner.pending.contains_key(&meta.path) {
                Some(Ok(()))
            } else {
                drop(inner);
                Some(this.launch(&meta).map(drop))
            }
        };
        match launched {
            Some(Err(error)) => {
                if let Some(restart) = this.lock().restarts.get_mut(&meta.path) {
                    restart.pending = false;
                }
                let reason = format!("Session worker {} failed to restart: {}", meta.id, error.message);
                this.schedule_restart(meta, Duration::ZERO, &reason);
            }
            Some(Ok(())) => {
                if let Some(restart) = this.lock().restarts.get_mut(&meta.path) {
                    restart.pending = false;
                }
                this.log("[ultron server info]", &format!("Session worker {} restarted", meta.id));
            }
            None => {
                this.lock().restarts.remove(&meta.path);
            }
        }
        this.changed.notify_all();
    }

    /// Restart `meta`'s worker after a backoff, retrying launch failures; gives up after [`MAX_RESTARTS`].
    fn schedule_restart(&self, meta: Meta, uptime: Duration, reason: &str) {
        let attempt = {
            let mut inner = self.lock();
            let restart = inner.restarts.entry(meta.path.clone()).or_insert(Restart {
                attempts: 0,
                pending: false,
            });
            if uptime >= RESTART_STABLE {
                restart.attempts = 0;
            }
            if restart.attempts >= MAX_RESTARTS {
                inner.restarts.remove(&meta.path);
                drop(inner);
                self.changed.notify_all();
                self.log(
                    "[ultron server error]",
                    &format!(
                        "Session worker {} crashed {MAX_RESTARTS} times in a row; not restarting it ({reason}). Its session is saved: attach to it again (`ultron --session-id {}`) once the cause is fixed",
                        meta.id, meta.id
                    ),
                );
                return;
            }
            restart.attempts += 1;
            restart.pending = true;
            restart.attempts
        };
        self.log(
            "[ultron server warning]",
            &format!(
                "{reason}; restarting it in {} ms (attempt {attempt} of {MAX_RESTARTS})",
                restart_delay(attempt).as_millis()
            ),
        );
        let this = self.me.clone();
        std::thread::spawn(move || Self::run_restart_attempt(this, meta, attempt));
    }

    fn coordinator(&self) -> &Arc<CoordinatorConnection> {
        self.coordinator
            .get()
            .expect("the coordinator connects before workers are used")
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        jsrt::sync::lock(&self.inner)
    }

    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&mut Inner) -> bool) -> (MutexGuard<'_, Inner>, bool) {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            if done(&mut inner) {
                return (inner, true);
            }
            let now = Instant::now();
            if now >= deadline {
                return (inner, false);
            }
            inner = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    fn notify_count(&self, inner: &Inner) {
        (self.on_count)(inner.workers.len() + inner.pending.len());
    }

    fn send(&self, peer: &str, payload: Value) -> std::io::Result<()> {
        self.coordinator().send(peer, payload)
    }

    /// The sessions of live workers (listed even before their files are).
    pub fn tracked_sessions(&self) -> Vec<Meta> {
        self.lock().workers.values().map(|w| w.meta.clone()).collect()
    }

    /// Session ID to worker PID.
    pub fn worker_pids(&self) -> HashMap<String, u32> {
        self.lock()
            .workers
            .values()
            .map(|w| (w.meta.id.clone(), w.pid))
            .collect()
    }

    /// Ask the coordinator's other workers who they belong to: orphans are adopted, owned ones answer so.
    pub fn discover(&self) {
        let peers = self.coordinator().peer_snapshot();
        let mut inner = self.lock();
        if inner.detached {
            return;
        }
        let undiscovered: HashSet<String> = peers
            .into_iter()
            .filter(|p| !inner.workers.contains_key(p) && !inner.pending.values().any(|q| &q.peer == p))
            .collect();
        if undiscovered.is_empty() {
            return;
        }
        inner.foreign.retain(|_, (peer, _)| !undiscovered.contains(peer));
        inner.discovery = Some(undiscovered);
        drop(inner);
        let _ = self.coordinator().broadcast(json!({"type": "discover_workers"}));
        let (mut inner, _) = self.wait_until(WORKER_DISCOVERY_TIMEOUT, |i| {
            i.discovery.as_ref().is_none_or(HashSet::is_empty)
        });
        inner.discovery = None;
    }

    fn mark_discovered(inner: &mut Inner, peer: &str) {
        if let Some(discovery) = inner.discovery.as_mut() {
            discovery.remove(peer);
        }
    }

    /// The options `meta`'s worker starts (or started) on.
    fn model_for(&self, path: &PathBuf) -> WorkerModel {
        self.session_models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .cloned()
            .unwrap_or_else(|| self.model.clone())
    }

    /// A client's session options for `meta`'s worker that is already running: refused unless they are the ones it
    /// started on (they cannot change a running worker).
    pub fn check_session_options(
        &self,
        meta_path: &PathBuf,
        session_id: &str,
        options: Option<&Value>,
    ) -> Result<(), ServiceError> {
        match options {
            Some(options) if self.model.with_session_options(options) != self.model_for(meta_path) => {
                Err(ServiceError::coded(
                    "service_invalid_value",
                    format!(
                        "Session {session_id} is already open on this server with other session options (model, \
                         thinking, prompt or skill flags): they apply when its worker starts. Detach its other \
                         clients and let it go idle first, or leave the flags out"
                    ),
                ))
            }
            _ => Ok(()),
        }
    }

    /// Open `meta`'s session with a client's session options: a worker started for it runs on them.
    pub fn open_session_with(&self, meta: &Meta, options: Option<&Value>) -> Result<Handle, ServiceError> {
        {
            let inner = self.lock();
            if inner.detached || inner.shutting_down {
                return Err(ServiceError::internal("Experimental server is shutting down"));
            }
        }
        if let Some(handle) = self.existing(meta) {
            self.check_session_options(&meta.path, &meta.id, options)?;
            return handle;
        }
        self.discover();
        if let Some(handle) = self.existing(meta) {
            self.check_session_options(&meta.path, &meta.id, options)?;
            return handle;
        }
        let foreign = self.lock().foreign.get(&meta.path).cloned();
        if let Some((peer, pid)) = foreign
            && self.coordinator().peer_snapshot().contains(&peer)
        {
            return Err(ServiceError::coded(
                "service_invalid_value",
                crate::worker::session_in_use(&meta.id, Some(pid)),
            ));
        }
        {
            let mut models = self
                .session_models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match options {
                Some(options) => models.insert(meta.path.clone(), self.model.with_session_options(options)),
                None => models.remove(&meta.path),
            };
        }
        self.launch(meta)
    }

    fn existing(&self, meta: &Meta) -> Option<Result<Handle, ServiceError>> {
        let inner = self.lock();
        if let Some(peer) = inner.by_session.get(&meta.path) {
            return Some(Ok(self.handle(&inner.workers[peer])));
        }
        let pending = inner.pending.get(&meta.path)?.peer.clone();
        drop(inner);
        Some(self.await_launch(&meta.path, &pending))
    }

    fn handle(&self, worker: &Worker) -> Handle {
        Handle {
            peer: worker.peer.clone(),
            session_id: worker.meta.id.clone(),
            path: worker.meta.path.clone(),
            terminated: worker.terminated.clone(),
        }
    }

    fn write_model_options(model: &WorkerModel, options: &mut Value) {
        if let Some(provider) = &model.provider {
            options["provider"] = json!(provider);
        }
        if let Some(id) = &model.model {
            options["model"] = json!(id);
        }
        if let Some(thinking) = &model.thinking {
            options["thinking"] = json!(thinking);
        }
        if let Some(mode) = &model.extension_mode {
            options["extensionMode"] = json!(mode);
        }
        if model.no_naming {
            options["noNaming"] = json!(true);
        }
        if let Some(models) = &model.models {
            options["models"] = json!(models);
        }
        model.session.write_worker_options(options);
    }

    fn worker_options(&self, meta: &Meta) -> Value {
        let mut options = json!({"sessionDir": self.session_dir.display().to_string(), "metadata": meta.to_json(), "pluginManifestPaths": []});
        Self::write_model_options(&self.model_for(&meta.path), &mut options);
        if let Some(defaults) = self.settings_defaults.get() {
            options["settingsDefaults"] = defaults.clone();
        }
        if self.durable.get().is_some() {
            options["durable"] = json!(true);
        }
        options
    }

    fn launch(&self, meta: &Meta) -> Result<Handle, ServiceError> {
        let peer = format!("worker-{}", jsrt::random::uuid_v4());
        let token = jsrt::random::uuid_v4();
        let key = jsrt::base64::encode_url(meta.path.display().to_string());
        let coordinator = self.coordinator();
        let mut env = vec![
            (
                crate::worker::CONTROL_ADDRESS_ENV.to_string(),
                coordinator.control_path.display().to_string(),
            ),
            (crate::worker::CONTROL_TOKEN_ENV.to_string(), token.clone()),
            (crate::worker::SESSION_KEY_ENV.to_string(), key),
            (crate::worker::PEER_ID_ENV.to_string(), peer.clone()),
            (
                crate::worker::OWNER_ENV.to_string(),
                coordinator.server_connection_id.clone(),
            ),
        ];
        let model = self.model_for(&meta.path);
        if let Some(api_key) = &model.api_key {
            env.push((crate::worker::API_KEY_ENV.to_string(), api_key.clone()));
        }
        env.extend(model.env.iter().cloned());
        if let Some(trusted) = model.project_trust {
            env.push((
                config::trust::TRUST_OVERRIDE_ENV.to_string(),
                if trusted { "1" } else { "0" }.to_string(),
            ));
        }
        crate::process::trace_startup("server.spawn-worker");
        let child = process::spawn(Role::SessionWorker, &[self.worker_options(meta).to_string()], &env)
            .map_err(|e| ServiceError::internal(e.to_string()))?;
        let pid = child.id();
        {
            let mut inner = self.lock();
            inner.pending.insert(
                meta.path.clone(),
                Pending {
                    peer: peer.clone(),
                    token,
                    pid,
                    outcome: None,
                    exited: None,
                },
            );
            self.notify_count(&inner);
        }
        let exited = process::watch(child);
        let (path, watch_peer) = (meta.path.clone(), peer.clone());
        let this = self.self_arc();
        std::thread::spawn(move || {
            let status = exited.recv().ok().flatten();
            if let Some(this) = this.and_then(|w| w.upgrade()) {
                this.child_exited(&path, &watch_peer, status);
            }
        });
        self.await_launch(&meta.path, &peer)
    }

    fn self_arc(&self) -> Option<std::sync::Weak<Manager>> {
        Some(self.me.clone())
    }

    fn await_launch(&self, path: &PathBuf, peer: &str) -> Result<Handle, ServiceError> {
        let (mut inner, ready) = self.wait_until(startup_timeout(path), |i| {
            i.pending
                .get(path)
                .is_none_or(|p| p.peer != peer || p.outcome.is_some())
        });
        if !ready {
            drop(inner);
            self.fail_pending(path, ServiceError::internal("Session worker startup timed out"));
            inner = self.lock();
        }
        match inner.pending.get(path) {
            Some(p) if p.peer == peer && p.outcome.is_some() => {
                let pending = inner.pending.remove(path).expect("checked above");
                self.notify_count(&inner);
                match pending.outcome.expect("checked above") {
                    Ok(peer) => inner
                        .workers
                        .get(&peer)
                        .map(|w| self.handle(w))
                        .ok_or_else(|| ServiceError::internal("Session worker disconnected during startup")),
                    Err(error) => Err(error),
                }
            }
            _ => match inner.by_session.get(path) {
                Some(peer) => Ok(self.handle(&inner.workers[peer])),
                None => Err(ServiceError::internal("Session worker startup failed")),
            },
        }
    }

    fn fail_pending(&self, path: &PathBuf, error: ServiceError) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(path)
            && pending.outcome.is_none()
        {
            pending.outcome = Some(Err(error));
            process::kill(pending.pid, rustix::process::Signal::KILL);
        }
        drop(inner);
        self.changed.notify_all();
    }

    pub fn attach_client(&self, handle: &Handle) -> Result<String, ServiceError> {
        {
            let inner = self.lock();
            let worker = inner.workers.get(&handle.peer);
            if inner.detached || inner.shutting_down {
                return Err(ServiceError::internal("Experimental Session worker is stopping"));
            }
            if worker.is_none_or(|w| w.stopping || w.retiring) {
                return Err(ServiceError::coded(
                    protocol::names::WORKER_RETIRING_CODE,
                    "Experimental Session worker is stopping",
                ));
            }
        }
        let attachment = jsrt::random::uuid_v4();
        if let Some(worker) = self.lock().workers.get_mut(&handle.peer) {
            worker.demand.attach(&attachment);
        }
        let outcome = self.apply_demand(&handle.peer, &attachment, true);
        self.settle_demand(&handle.peer, &attachment, true, &outcome);
        if let Err(error) = outcome {
            if error == DemandError::TimedOut {
                self.withdraw(&handle.peer, &attachment);
            }
            let error = error.to_string();
            if error.ends_with(RETIRING_REJECTION) || handle.terminated.is_resolved() {
                return Err(ServiceError::coded(protocol::names::WORKER_RETIRING_CODE, error));
            }
            return Err(ServiceError::internal(error));
        }
        Ok(attachment)
    }

    pub fn release_attachment(&self, handle: &Handle, attachment: &str) {
        let owed = {
            let mut inner = self.lock();
            !inner.detached
                && inner
                    .workers
                    .get_mut(&handle.peer)
                    .is_some_and(|w| w.demand.withdraw(attachment))
        };
        if owed {
            self.withdraw(&handle.peer, attachment);
        }
        let server = self.coordinator().server_connection_id.clone();
        let mut inner = self.lock();
        let prefix = format!("{server}\0{attachment}\0");
        inner.subscriptions.retain(|key, _| !key.starts_with(&prefix));
    }

    /// Tell the worker `attachment` is released; a detach it does not confirm stays owed and is sent again with the
    /// heartbeat ([`DemandLedger::owed`]).
    fn withdraw(&self, peer: &str, attachment: &str) {
        let outcome = self.apply_demand(peer, attachment, false);
        self.settle_demand(peer, attachment, false, &outcome);
    }

    fn settle_demand(&self, peer: &str, attachment: &str, attached: bool, outcome: &Result<(), DemandError>) {
        if let Some(worker) = self.lock().workers.get_mut(peer) {
            worker.demand.settle(attachment, attached, outcome);
        }
    }

    fn demand_message(&self, request: &str, attachment: &str, attached: bool) -> Value {
        let server = self.coordinator().server_connection_id.clone();
        json!({"type": "session_demand", "serverConnectionId": server, "requestId": request, "attachmentId": attachment, "attached": attached})
    }

    fn apply_demand(&self, peer: &str, attachment: &str, attached: bool) -> Result<(), DemandError> {
        let request = jsrt::random::uuid_v4();
        {
            let mut inner = self.lock();
            if inner.workers.get(peer).is_none_or(|w| w.stopping) {
                return Err(DemandError::Failed("Experimental Session worker is stopping".into()));
            }
            inner.demands.insert(request.clone(), (peer.to_string(), None));
        }
        if let Err(error) = self.send(peer, self.demand_message(&request, attachment, attached)) {
            self.lock().demands.remove(&request);
            return Err(DemandError::Failed(error.to_string()));
        }
        let (mut inner, _) = self.wait_until(WORKER_DEMAND_TIMEOUT, |i| {
            i.demands.get(&request).is_none_or(|(_, outcome)| outcome.is_some())
        });
        let outcome = inner.demands.remove(&request);
        drop(inner);
        match outcome {
            Some((_, Some(result))) => result.map_err(DemandError::Failed),
            None => Err(DemandError::Failed(
                "Session worker disconnected during demand update".into(),
            )),
            Some((_, None)) => Err(DemandError::TimedOut),
        }
    }

    fn wrap_operation_done(
        this: Option<std::sync::Weak<Manager>>,
        sub_key: Option<String>,
        unsubscribe: Option<String>,
        done: Done,
    ) -> Done {
        Box::new(move |result: ServiceResult| {
            if let Some(this) = this.and_then(|w| w.upgrade()) {
                let mut inner = this.lock();
                if result.is_err()
                    && let Some(key) = &sub_key
                {
                    inner.subscriptions.remove(key);
                }
                if result.is_ok()
                    && let Some(key) = &unsubscribe
                {
                    inner.subscriptions.remove(key);
                }
            }
            done(result);
        })
    }

    /// Route one service operation on an attachment; `done` runs with its result (on the coordinator reader).
    /// Returns the operation's request id once it was sent ([`Manager::cancel_operation`] cancels it).
    pub fn invoke(
        &self,
        handle: &Handle,
        attachment: &str,
        call: Value,
        publish: Publish,
        done: Done,
    ) -> Option<String> {
        let server = self.coordinator().server_connection_id.clone();
        let scope = json!({"serverConnectionId": server, "attachmentId": attachment});
        let mut inner = self.lock();
        let valid = !inner.detached
            && !inner.shutting_down
            && inner
                .workers
                .get(&handle.peer)
                .is_some_and(|w| !w.stopping && w.demand.is_attached(attachment));
        if !valid {
            drop(inner);
            done(Err(ServiceError::internal(
                "Experimental Session worker has no active attachment",
            )));
            return None;
        }
        let subscription = subscribe_id(&call);
        if let Some(id) = &subscription {
            let key = scoped(&scope, id);
            if inner.subscriptions.contains_key(&key) {
                drop(inner);
                done(Err(ServiceError::internal("Service subscription ID is already active")));
                return None;
            }
            inner.subscriptions.insert(key, (handle.peer.clone(), publish));
        }
        let unsubscribe = unsubscribe_id(&call).map(|id| scoped(&scope, &id));
        let request = jsrt::random::uuid_v4();
        let sub_key = subscription.map(|id| scoped(&scope, &id));
        let wrapped = Self::wrap_operation_done(self.self_arc(), sub_key, unsubscribe, done);
        inner
            .operations
            .insert(request.clone(), (handle.peer.clone(), scope.clone(), wrapped));
        drop(inner);
        let sent = self.send(
            &handle.peer,
            json!({"type": "operation", "requestId": request, "scope": scope, "call": call}),
        );
        if let Err(error) = sent {
            self.reject_operation(&request, error.to_string());
            return None;
        }
        Some(request)
    }

    /// The caller stopped waiting for operation `request` (its client cancelled the call or disconnected): the worker
    /// is told (`operation_cancel`, its call's context is cancelled) and the operation settles now as cancelled.
    /// An operation that already settled is left alone.
    pub fn cancel_operation(&self, request: &str) {
        let removed = self.lock().operations.remove(request);
        let Some((peer, scope, done)) = removed else {
            return;
        };
        let _ = self.send(
            &peer,
            json!({"type": "operation_cancel", "requestId": request, "scope": scope}),
        );
        done(Err(ServiceError::coded(CANCELLED_CODE, CANCELLED_MESSAGE)));
    }

    fn reject_operation(&self, request: &str, message: String) {
        let removed = self.lock().operations.remove(request);
        if let Some((_, _, done)) = removed {
            done(Err(ServiceError::internal(message)));
        }
    }

    /// `closeSession`: stop the worker, or release it while the server shuts down.
    pub fn close_session(&self, handle: &Handle) {
        if self.lock().releasing {
            self.release_worker(&handle.peer);
        } else {
            self.stop_worker(&handle.peer);
        }
    }

    pub fn stop_session(&self, meta: &Meta) {
        let peer = self.lock().by_session.get(&meta.path).cloned();
        if let Some(peer) = peer {
            self.stop_worker(&peer);
        }
    }

    fn stop_worker(&self, peer: &str) {
        let terminated = {
            let mut inner = self.lock();
            if inner.detached {
                return;
            }
            let Some(worker) = inner.workers.get_mut(peer) else {
                return;
            };
            worker.stopping = true;
            worker.expected_stop = true;
            worker.terminated.clone()
        };
        self.reject_worker_operations(peer, "Session worker is stopping");
        let _ = self.send(peer, json!({"type": "shutdown"}));
        if !terminated.wait(WORKER_SHUTDOWN_TIMEOUT) {
            let pid = self.lock().workers.get(peer).map(|w| w.pid);
            if let Some(pid) = pid {
                process::kill(pid, rustix::process::Signal::KILL);
            }
            self.remove_worker(peer, None);
        }
    }

    /// Hand a busy worker off (it keeps running detached); an idle one exits.
    fn release_worker(&self, peer: &str) {
        let terminated = {
            let mut inner = self.lock();
            if inner.detached {
                return;
            }
            let Some(worker) = inner.workers.get_mut(peer) else {
                return;
            };
            worker.expected_stop = true;
            worker.terminated.clone()
        };
        self.reject_worker_operations(peer, "Experimental server is shutting down");
        let request = jsrt::random::uuid_v4();
        self.lock()
            .releases
            .insert(request.clone(), (peer.to_string(), None, false));
        let server = self.coordinator().server_connection_id.clone();
        if self
            .send(
                peer,
                json!({"type": "release", "requestId": request, "serverConnectionId": server}),
            )
            .is_err()
            && let Some(release) = self.lock().releases.get_mut(&request)
        {
            release.1 = Some(false);
            release.2 = true;
        }
        let terminated_wait = terminated.clone();
        let (mut inner, _) = self.wait_until(WORKER_SHUTDOWN_TIMEOUT, |i| {
            i.releases.get(&request).is_none_or(|r| r.2) || terminated_wait.is_resolved()
        });
        let retained = inner.releases.remove(&request).and_then(|r| r.1);
        drop(inner);
        let retained = if terminated.is_resolved() {
            Some(false)
        } else {
            retained
        };
        match retained {
            Some(true) => self.forget_and_resolve(peer, None),
            Some(false) => {
                if let Some(worker) = self.lock().workers.get_mut(peer) {
                    worker.stopping = true;
                }
                if !terminated.wait(WORKER_SHUTDOWN_TIMEOUT) {
                    self.stop_worker(peer);
                }
            }
            None => self.stop_worker(peer),
        }
    }

    pub fn prepare_shutdown(&self) {
        self.lock().releasing = true;
    }

    /// Release every worker (busy ones stay running detached) and forget them.
    pub fn shutdown(&self) {
        let peers: Vec<String> = {
            let mut inner = self.lock();
            if inner.detached || inner.shutting_down {
                return;
            }
            inner.releasing = true;
            for pending in inner.pending.values() {
                process::kill(pending.pid, rustix::process::Signal::KILL);
            }
            inner.workers.keys().cloned().collect()
        };
        let threads: Vec<_> = peers
            .into_iter()
            .map(|peer| {
                let this = self.self_arc();
                std::thread::spawn(move || {
                    if let Some(this) = this.and_then(|w| w.upgrade()) {
                        this.release_worker(&peer);
                    }
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
        self.lock().shutting_down = true;
        self.detach_state();
    }

    /// Forget workers without stopping them (the coordinator was lost).
    pub fn detach(&self) {
        let mut inner = self.lock();
        if inner.detached {
            return;
        }
        inner.detached = true;
        for pending in inner.pending.values_mut() {
            pending
                .outcome
                .get_or_insert(Err(ServiceError::internal("Ultron server lost its coordinator")));
        }
        drop(inner);
        self.detach_state();
    }

    fn detach_state(&self) {
        let mut inner = self.lock();
        inner.detached = true;
        let operations: Vec<Done> = inner.operations.drain().map(|(_, (_, _, done))| done).collect();
        for (_, outcome) in inner.demands.values_mut() {
            outcome.get_or_insert(Ok(()));
        }
        for release in inner.releases.values_mut() {
            release.2 = true;
        }
        inner.foreign.clear();
        inner.subscriptions.clear();
        inner.workers.clear();
        inner.by_session.clear();
        inner.discovery = None;
        drop(inner);
        self.changed.notify_all();
        for done in operations {
            done(Err(ServiceError::internal(
                "Experimental server detached during a worker operation",
            )));
        }
    }

    fn reject_worker_operations(&self, peer: &str, message: &str) {
        self.reject_worker_operations_with(peer, ServiceError::internal(message));
    }

    fn reject_worker_operations_with(&self, peer: &str, error: ServiceError) {
        let rejected: Vec<Done> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .operations
                .iter()
                .filter(|(_, (p, _, _))| p == peer)
                .map(|(k, _)| k.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| inner.operations.remove(&id))
                .map(|(_, _, done)| done)
                .collect()
        };
        for done in rejected {
            done(Err(error.clone()));
        }
    }

    fn remove_worker(&self, peer: &str, error: Option<String>) {
        self.forget_and_resolve(peer, error);
    }

    fn forget_and_resolve(&self, peer: &str, error: Option<String>) {
        if let Some(worker) = self.forget(peer, error.as_deref()) {
            self.resolve_lost(worker, error);
        }
    }

    /// Resolve a forgotten worker's termination; an unexpected end of a worker with work restarts it first (so
    /// the router sees the restart under way and keeps its clients).
    fn resolve_lost(&self, worker: Worker, error: Option<String>) {
        if let Some(reason) = &error
            && Self::should_restart(&self.lock(), &worker)
        {
            self.schedule_restart(worker.meta.clone(), worker.started.elapsed(), reason);
        }
        worker.terminated.resolve(error);
    }

    /// Drop a worker from routing and fail its in-flight work (`lost`: it ended unexpectedly, with this reason).
    fn forget(&self, peer: &str, lost: Option<&str>) -> Option<Worker> {
        let mut inner = self.lock();
        let worker = inner.workers.remove(peer)?;
        inner.by_session.remove(&worker.meta.path);
        inner.subscriptions.retain(|_, (p, _)| p != peer);
        for (owner, outcome) in inner.demands.values_mut() {
            if owner == peer {
                outcome.get_or_insert(Err("Session worker disconnected during demand update".into()));
            }
        }
        self.notify_count(&inner);
        drop(inner);
        self.changed.notify_all();
        match lost {
            Some(reason) => self.reject_worker_operations_with(
                peer,
                ServiceError::coded(
                    protocol::names::WORKER_LOST_CODE,
                    format!("{reason} during an operation"),
                ),
            ),
            None => self.reject_worker_operations(peer, "Session worker disconnected during an operation"),
        }
        Some(worker)
    }

    fn describe_exit(status: Option<std::process::ExitStatus>) -> String {
        status.map_or("unknown".to_string(), |s| {
            use std::os::unix::process::ExitStatusExt;
            match (s.code(), s.signal()) {
                (Some(code), _) => code.to_string(),
                (None, Some(signal)) => format!("signal {signal}"),
                _ => "unknown".into(),
            }
        })
    }

    fn handle_pending_exit(&self, path: &PathBuf, peer: &str, describe: &str) -> bool {
        let pending_here = self
            .lock()
            .pending
            .get(path)
            .is_some_and(|p| p.peer == peer && p.outcome.is_none());
        if !pending_here {
            return false;
        }
        let error = ServiceError::internal(format!("Session worker exited before readiness ({describe})"));
        if self.coordinator().peer_snapshot().contains(peer) {
            let mut inner = self.lock();
            if let Some(pending) = inner.pending.get_mut(path)
                && pending.peer == peer
                && pending.outcome.is_none()
            {
                pending.pid = 0;
                pending.exited = Some(error);
            }
            return true;
        }
        self.fail_pending(path, error);
        true
    }

    fn defer_clean_exit(&self, peer: &str) {
        let Some(worker) = self.forget(peer, None) else { return };
        let terminated = worker.terminated.clone();
        self.lock().exited.insert(peer.to_string(), worker);
        let this = self.self_arc();
        let peer = peer.to_string();
        std::thread::spawn(move || {
            if !terminated.wait(WORKER_SHUTDOWN_TIMEOUT)
                && let Some(this) = this.and_then(|w| w.upgrade())
            {
                this.settle_exited(&peer);
            }
        });
    }

    fn child_exited(&self, path: &PathBuf, peer: &str, status: Option<std::process::ExitStatus>) {
        let describe = Self::describe_exit(status);
        if self.handle_pending_exit(path, peer, &describe) {
            return;
        }
        let inner = self.lock();
        let Some(worker) = inner.workers.get(peer) else { return };
        let unexpected = format!("Session worker {} exited unexpectedly ({describe})", worker.meta.id);
        let clean = status.is_some_and(|s| s.success());
        let (expected, retiring) = (worker.expected_stop, worker.retiring);
        drop(inner);
        if expected || !clean || retiring {
            let error = (!(expected || (clean && retiring))).then_some(unexpected);
            self.remove_worker(peer, error);
            return;
        }
        self.defer_clean_exit(peer);
    }

    fn settle_exited(&self, peer: &str) {
        let worker = self.lock().exited.remove(peer);
        if let Some(worker) = worker {
            let error = (!(worker.expected_stop || worker.retiring))
                .then(|| format!("Session worker {} exited unexpectedly (0)", worker.meta.id));
            self.resolve_lost(worker, error);
        }
    }
}
