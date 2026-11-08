//! A worker thread fed jobs over a channel, for the services that keep blocking work (clipboard, git, desktop
//! notifications, network lookups) off the paint path. Dropping the service stops the worker after the job it is
//! on; [`ServiceThread::shutdown`] also waits for it.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

enum Msg<J> {
    Job(J),
    Stop,
}

/// The jobs a worker receives: they end at a stop or when every sender is gone.
pub struct Jobs<J> {
    rx: Receiver<Msg<J>>,
    stopped: bool,
}

impl<J> Jobs<J> {
    /// The next queued job without waiting (to coalesce a burst).
    pub fn try_next(&mut self) -> Option<J> {
        if self.stopped {
            return None;
        }
        match self.rx.try_recv() {
            Ok(Msg::Job(job)) => Some(job),
            Ok(Msg::Stop) => {
                self.stopped = true;
                None
            }
            Err(_) => None,
        }
    }
}

impl<J> Iterator for Jobs<J> {
    type Item = J;

    fn next(&mut self) -> Option<J> {
        if self.stopped {
            return None;
        }
        match self.rx.recv() {
            Ok(Msg::Job(job)) => Some(job),
            Ok(Msg::Stop) | Err(_) => {
                self.stopped = true;
                None
            }
        }
    }
}

/// Queues jobs for a [`ServiceThread`]; a send after the worker stopped is dropped.
pub struct JobSender<J>(Sender<Msg<J>>);

impl<J> Clone for JobSender<J> {
    fn clone(&self) -> Self {
        JobSender(self.0.clone())
    }
}

impl<J> JobSender<J> {
    pub fn send(&self, job: J) {
        let _ = self.0.send(Msg::Job(job));
    }
}

pub struct ServiceThread<J> {
    tx: JobSender<J>,
    worker: Option<JoinHandle<()>>,
}

impl<J: Send + 'static> ServiceThread<J> {
    /// Start the thread `name` running `run` over the jobs sent to it.
    pub fn spawn(name: &str, run: impl FnOnce(Jobs<J>) + Send + 'static) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let worker = thread::Builder::new()
            .name(name.to_string())
            .spawn(move || run(Jobs { rx, stopped: false }))?;
        Ok(ServiceThread {
            tx: JobSender(tx),
            worker: Some(worker),
        })
    }
}

impl<J> ServiceThread<J> {
    pub fn send(&self, job: J) {
        self.tx.send(job);
    }

    /// A sender for code that outlives a borrow of the service (a yank sink).
    pub fn sender(&self) -> JobSender<J> {
        self.tx.clone()
    }

    /// Finish the queued jobs, then stop and wait for the worker.
    pub fn shutdown(mut self) {
        let _ = self.tx.0.send(Msg::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<J> Drop for ServiceThread<J> {
    fn drop(&mut self) {
        let _ = self.tx.0.send(Msg::Stop);
    }
}
