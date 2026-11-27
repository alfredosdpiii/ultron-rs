use super::{DemandError, DemandLedger, Inner, Manager, Terminated, Worker, scoped};
use crate::catalog::Meta;
use crate::chord::ServiceError;
use crate::coord_client::Event;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

impl Manager {
    /// Coordinator events, on the coordinator connection's reader thread.
    pub fn handle_event(&self, event: Event) {
        if self.lock().detached {
            return;
        }
        match event {
            Event::PeerConnected(_) => {}
            Event::PeerDisconnected(peer) => {
                let mut inner = self.lock();
                Self::mark_discovered(&mut inner, &peer);
                inner.foreign.retain(|_, (p, _)| *p != peer);
                let worker = inner
                    .workers
                    .get(&peer)
                    .map(|w| (w.expected_stop || w.retiring, w.meta.id.clone()));
                let pending = inner
                    .pending
                    .iter_mut()
                    .find(|(_, p)| p.peer == peer)
                    .map(|(k, p)| (k.clone(), p.exited.take()));
                drop(inner);
                self.changed.notify_all();
                self.settle_exited(&peer);
                if let Some((expected, id)) = worker {
                    let error = (!expected).then(|| format!("Session worker {id} disconnected unexpectedly"));
                    self.remove_worker(&peer, error);
                }
                if let Some((path, exited)) = pending {
                    let error =
                        exited.unwrap_or_else(|| ServiceError::internal("Session worker disconnected during startup"));
                    self.fail_pending(&path, error);
                }
            }
            Event::Message { from, payload } => self.handle_message(&from, &payload),
        }
    }

    fn owns_worker(inner: &Inner, from: &str, token: &str, path: &PathBuf) -> bool {
        inner
            .workers
            .get(from)
            .is_some_and(|w| w.token == token && w.meta.path == *path)
    }

    fn message_key(message: &Value) -> PathBuf {
        PathBuf::from(message["sessionKey"].as_str().unwrap_or_default())
    }

    fn handle_worker_owned(inner: &mut Inner, from: &str, message: &Value) {
        if !inner.workers.contains_key(from) {
            let pid = message["pid"].as_u64().unwrap_or(0) as u32;
            inner
                .foreign
                .insert(Self::message_key(message), (from.to_string(), pid));
        }
        Self::mark_discovered(inner, from);
    }

    fn handle_worker_failed(&self, inner: std::sync::MutexGuard<'_, Inner>, from: &str, token: &str, message: &Value) {
        let path = Self::message_key(message);
        if !inner
            .pending
            .get(&path)
            .is_some_and(|p| p.peer == from && p.token == token)
        {
            drop(inner);
            self.changed.notify_all();
            return;
        }
        drop(inner);
        let text = message["message"].as_str().unwrap_or_default();
        let error = match message["code"].as_str() {
            Some(code) => ServiceError::coded(code, text),
            None => ServiceError::internal(format!("Session worker failed: {text}")),
        };
        self.fail_pending(&path, error);
    }

    fn handle_demand_response(inner: &mut Inner, from: &str, token: &str, kind: &str, message: &Value) {
        let request = message["requestId"].as_str().unwrap_or_default().to_string();
        let path = Self::message_key(message);
        let valid = Self::owns_worker(inner, from, token, &path);
        if inner.demands.get(&request).is_some_and(|(p, _)| p == from) && valid {
            let outcome = if kind == "demand_applied" {
                Ok(())
            } else {
                Err(format!(
                    "Session worker rejected demand: {}",
                    message["message"].as_str().unwrap_or_default()
                ))
            };
            if let Some(entry) = inner.demands.get_mut(&request) {
                entry.1 = Some(outcome);
            }
        }
        if message["attached"] == false
            && valid
            && let (Some(attachment), Some(worker)) = (message["attachmentId"].as_str(), inner.workers.get_mut(from))
        {
            let outcome = match kind {
                "demand_applied" => Ok(()),
                _ => Err(DemandError::Failed(String::new())),
            };
            worker.demand.settle(attachment, false, &outcome);
        }
    }

    fn handle_worker_retiring(inner: &mut Inner, from: &str, token: &str, message: &Value) {
        let path = Self::message_key(message);
        if let Some(worker) = inner.workers.get_mut(from)
            && worker.token == token
            && worker.meta.path == path
        {
            worker.retiring = true;
        }
        if let Some(worker) = inner.exited.get_mut(from)
            && worker.token == token
        {
            worker.retiring = true;
        }
    }

    fn handle_worker_released(inner: &mut Inner, from: &str, token: &str, message: &Value) {
        let request = message["requestId"].as_str().unwrap_or_default().to_string();
        let path = Self::message_key(message);
        let valid = Self::owns_worker(inner, from, token, &path);
        if let Some(release) = inner.releases.get_mut(&request)
            && release.0 == from
            && valid
        {
            release.1 = message["retained"].as_bool();
            release.2 = true;
        }
    }

    fn handle_operation_response(
        &self,
        mut inner: std::sync::MutexGuard<'_, Inner>,
        from: &str,
        token: &str,
        message: &Value,
    ) {
        let response = &message["response"];
        let request = response["requestId"].as_str().unwrap_or_default().to_string();
        let path = Self::message_key(message);
        let valid = Self::owns_worker(&inner, from, token, &path);
        let Some((peer, scope, done)) = inner.operations.remove(&request) else {
            return;
        };
        drop(inner);
        let result = if peer != from || !valid || response["scope"] != scope {
            Err(ServiceError::internal(
                "Session worker returned a mismatched operation response",
            ))
        } else if response["type"] == "operation_error" {
            let text = response["message"].as_str().unwrap_or_default();
            Err(match response["code"].as_str() {
                Some(code) => ServiceError::coded(code, text),
                None => ServiceError::internal(format!("Session worker operation failed: {text}")),
            })
        } else {
            Ok(response.get("result").cloned())
        };
        done(result);
    }

    fn handle_service_update(&self, inner: std::sync::MutexGuard<'_, Inner>, from: &str, token: &str, message: &Value) {
        let subscription = message["subscriptionId"].as_str().unwrap_or_default();
        let path = Self::message_key(message);
        let valid = Self::owns_worker(&inner, from, token, &path);
        let entry = inner
            .subscriptions
            .get(&scoped(&message["scope"], subscription))
            .cloned();
        drop(inner);
        if let Some((peer, publish)) = entry
            && peer == from
            && valid
        {
            publish(subscription, message["update"].clone());
        }
    }

    fn handle_worker_schedules(
        &self,
        inner: std::sync::MutexGuard<'_, Inner>,
        from: &str,
        token: &str,
        message: &Value,
    ) {
        let path = Self::message_key(message);
        let meta = inner
            .workers
            .get(from)
            .filter(|_| Self::owns_worker(&inner, from, token, &path))
            .map(|w| w.meta.clone());
        drop(inner);
        if let (Some(meta), Some(listener)) = (meta, self.schedules.get()) {
            listener(&meta, &message["schedules"]);
        }
    }

    fn handle_worker_status(
        &self,
        mut inner: std::sync::MutexGuard<'_, Inner>,
        from: &str,
        token: &str,
        message: &Value,
    ) {
        let path = Self::message_key(message);
        if !Self::owns_worker(&inner, from, token, &path) {
            return;
        }
        let health = &message["health"];
        let stuck = health["stuck"] == true;
        let Some(worker) = inner.workers.get_mut(from) else {
            return;
        };
        worker.busy = message["busy"] == true;
        let newly_stuck = stuck && !worker.stuck;
        worker.stuck = stuck;
        let id = worker.meta.id.clone();
        drop(inner);
        if newly_stuck {
            self.log(
                "[ultron server warning]",
                &format!(
                    "Session {id}: run {} made no progress for {} ms (stuck); abort it with ultron.session-ops abortStuck",
                    health["runId"].as_str().unwrap_or_default(),
                    health["idleMs"]
                ),
            );
        }
    }

    fn handle_state_message(inner: &mut Inner, kind: &str, from: &str, token: &str, message: &Value) -> bool {
        match kind {
            "worker_owned" => Self::handle_worker_owned(inner, from, message),
            "demand_applied" | "demand_rejected" => Self::handle_demand_response(inner, from, token, kind, message),
            "worker_retiring" => Self::handle_worker_retiring(inner, from, token, message),
            "worker_released" => Self::handle_worker_released(inner, from, token, message),
            _ => return false,
        }
        true
    }

    fn handle_message(&self, from: &str, message: &Value) {
        let token = message["token"].as_str().unwrap_or_default();
        let mut inner = self.lock();
        if let Some(worker) = inner.workers.get_mut(from) {
            worker.last_seen = Instant::now();
        }
        let kind = message["type"].as_str().unwrap_or_default();
        if Self::handle_state_message(&mut inner, kind, from, token, message) {
            drop(inner);
            self.changed.notify_all();
            return;
        }
        match kind {
            "worker_failed" => self.handle_worker_failed(inner, from, token, message),
            "operation_response" => self.handle_operation_response(inner, from, token, message),
            "service_update" => self.handle_service_update(inner, from, token, message),
            "worker_schedules" => self.handle_worker_schedules(inner, from, token, message),
            "worker_ready" => {
                drop(inner);
                self.record_ready(from, message);
            }
            "worker_status" => self.handle_worker_status(inner, from, token, message),
            _ => {
                drop(inner);
                self.changed.notify_all();
            }
        }
    }

    fn parse_ready_meta(message: &Value) -> Option<(Meta, PathBuf)> {
        let meta = Meta::from_json(&message["metadata"])?;
        let path = Self::message_key(message);
        let valid = path == meta.path
            && message["sessionId"] == json!(meta.id)
            && std::path::Path::new(&meta.cwd).is_absolute()
            && meta.path.is_absolute();
        valid.then_some((meta, path))
    }

    fn pending_mismatch(inner: &Inner, path: &PathBuf, peer: &str, token: &str, pid: u32) -> bool {
        inner
            .pending
            .get(path)
            .is_some_and(|pending| pending.peer != peer || pending.token != token || pending.pid != pid)
    }

    fn record_ready(&self, peer: &str, message: &Value) {
        let Some((meta, path)) = Self::parse_ready_meta(message) else {
            return;
        };
        let token = message["token"].as_str().unwrap_or_default().to_string();
        let pid = message["pid"].as_u64().unwrap_or(0) as u32;
        let mut inner = self.lock();
        Self::mark_discovered(&mut inner, peer);
        if inner.foreign.get(&path).is_some_and(|(p, _)| p == peer) {
            inner.foreign.remove(&path);
        }
        if inner.by_session.get(&path).is_some_and(|existing| existing != peer) {
            drop(inner);
            let _ = self.send(peer, json!({"type": "shutdown"}));
            return;
        }
        if Self::pending_mismatch(&inner, &path, peer, &token, pid) || inner.by_session.contains_key(&path) {
            return;
        }
        inner.workers.insert(
            peer.to_string(),
            Worker {
                peer: peer.to_string(),
                meta,
                pid,
                token,
                demand: DemandLedger::default(),
                expected_stop: false,
                retiring: false,
                stopping: false,
                terminated: Arc::new(Terminated::default()),
                busy: message["busy"] == true,
                stuck: false,
                last_seen: Instant::now(),
                started: Instant::now(),
            },
        );
        inner.by_session.insert(path.clone(), peer.to_string());
        if let Some(pending) = inner.pending.get_mut(&path) {
            pending.outcome = Some(Ok(peer.to_string()));
        }
        self.notify_count(&inner);
        drop(inner);
        self.changed.notify_all();
    }
}
