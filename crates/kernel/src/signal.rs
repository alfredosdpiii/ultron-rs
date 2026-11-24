//! Cancellation signals ([`Signal`]) and listener guards ([`Link`]) for kernel calls and host requests.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

type Listener = Box<dyn FnOnce(String) + Send>;

#[derive(Default)]
struct SignalState {
    reason: Option<String>,
    listeners: Vec<(u64, Listener)>,
    next: u64,
}

/// Cancellation, like an `AbortSignal`: aborted once, with a reason. A call's signal cancels it; the signal a
/// [`HostHandler`] receives is aborted when its cell completes or fails.
#[derive(Clone, Default)]
pub struct Signal {
    inner: Arc<(Mutex<SignalState>, Condvar)>,
}

impl Signal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Aborts with `reason`; later calls do nothing.
    pub fn abort(&self, reason: impl Into<String>) {
        let reason = reason.into();
        let listeners = {
            let mut state = jsrt::sync::lock(&self.inner.0);
            if state.reason.is_some() {
                return;
            }
            state.reason = Some(reason.clone());
            std::mem::take(&mut state.listeners)
        };
        self.inner.1.notify_all();
        for (_, listener) in listeners {
            listener(reason.clone());
        }
    }

    pub fn reason(&self) -> Option<String> {
        jsrt::sync::lock(&self.inner.0).reason.clone()
    }

    pub fn is_aborted(&self) -> bool {
        jsrt::sync::lock(&self.inner.0).reason.is_some()
    }

    /// Blocks until aborted or `timeout` passes; whether it is aborted.
    pub fn wait(&self, timeout: Duration) -> bool {
        let state = jsrt::sync::lock(&self.inner.0);
        let (state, _) = self
            .inner
            .1
            .wait_timeout_while(state, timeout, |state| state.reason.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.reason.is_some()
    }

    /// Runs `listener` once, on abort (on the aborting thread); None (and `listener` is dropped) when already
    /// aborted. [`Signal::forget`] removes it.
    pub fn on_abort(&self, listener: impl FnOnce(String) + Send + 'static) -> Option<u64> {
        self.listen(Box::new(listener)).ok()
    }

    pub fn forget(&self, id: u64) {
        jsrt::sync::lock(&self.inner.0)
            .listeners
            .retain(|(listener, _)| *listener != id);
    }

    /// Runs `listener` once on abort, or at once (on this thread) with the reason when already aborted, until the
    /// returned guard drops.
    pub fn link(&self, listener: impl FnOnce(String) + Send + 'static) -> Link {
        let id = match self.listen(Box::new(listener)) {
            Ok(id) => Some(id),
            Err((listener, reason)) => {
                listener(reason);
                None
            }
        };
        Link {
            signal: self.clone(),
            id,
        }
    }

    /// Adds `listener`; it comes back with the reason when already aborted.
    fn listen(&self, listener: Listener) -> Result<u64, (Listener, String)> {
        let mut state = jsrt::sync::lock(&self.inner.0);
        if let Some(reason) = state.reason.clone() {
            return Err((listener, reason));
        }
        state.next += 1;
        let id = state.next;
        state.listeners.push((id, listener));
        Ok(id)
    }

    /// [`signals::child::run`], cancelled when this signal aborts (at once when it already has).
    pub fn run_child(
        &self,
        command: &mut std::process::Command,
        input: Option<&[u8]>,
        bound: signals::child::Bound,
    ) -> std::io::Result<signals::child::Output> {
        let running = signals::child::start(command, input, bound)?;
        let watched = running.watched().clone();
        let _link = self.link(move |_| watched.cancel());
        Ok(running.finish())
    }
}

/// A [`Signal::link`] listener, removed when this drops.
pub struct Link {
    signal: Signal,
    id: Option<u64>,
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.signal.forget(id);
        }
    }
}
