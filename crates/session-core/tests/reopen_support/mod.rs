//! A session actor started, closed and opened again in-process over the same private home and session file, as a
//! successor worker opens a session (no process is killed and no signal is sent: the first actor hands the session
//! over, or closes, through its own input). Built on `p_session_support` (its scripted loopback server and replies).

use super::p_session_support::{HANG, Log, Script, server};
use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, OnceLock};

/// Where a session lives: a private home with its agent directory, working directory and session file.
pub struct Home {
    pub root: PathBuf,
    pub cwd: PathBuf,
    _tmp: tempfile::TempDir,
}

/// What a session's events are handed to as they are emitted (on the actor's thread), with the actor's input.
pub type Watcher = Box<dyn FnMut(&Value, &Sender<Input>) + Send>;

/// One actor over the session.
pub struct Worker {
    pub tx: Sender<Input>,
    pub events: Arc<Log<Value>>,
    pub requests: Arc<Log<Value>>,
    actor: Option<std::thread::JoinHandle<()>>,
}

impl Home {
    pub fn new(settings: Option<Value>) -> Home {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let cwd = root.join("cwd");
        std::fs::create_dir_all(root.join("agent")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        if let Some(settings) = settings {
            std::fs::write(root.join("agent").join("settings.json"), settings.to_string()).unwrap();
        }
        let home = Home { root, cwd, _tmp: tmp };
        let repo = session_journal::SessionRepo::new(&home.sessions(), session_journal::SystemClock::shared());
        drop(repo.create(&home.cwd.display().to_string(), None, None).unwrap());
        home
    }

    pub fn sessions(&self) -> PathBuf {
        self.root.join("agent").join("experimental").join("sessions")
    }

    /// The committed writes of the session's journal, in order.
    pub fn writes(&self) -> Vec<Value> {
        let mut files = Vec::new();
        let mut stack = vec![self.sessions()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(path);
                }
            }
        }
        assert_eq!(files.len(), 1, "{files:?}");
        std::fs::read_to_string(&files[0])
            .unwrap()
            .lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
            .collect()
    }

    /// The values set in `namespace`, in order.
    pub fn values(&self, namespace: &str) -> Vec<Value> {
        self.writes()
            .into_iter()
            .filter(|w| w["namespace"] == namespace && w["op"] == "set")
            .map(|w| w["value"].clone())
            .collect()
    }

    /// Open the session in an actor answered by `script` (a fresh loopback server that models.json points at);
    /// `watch` sees every event as the actor emits it.
    pub fn open(&self, script: Script, vars: &[(&str, &str)], watch: Option<Watcher>) -> Worker {
        self.try_open(script, vars, watch).unwrap()
    }

    /// [`Home::open`], returning the actor's open error (a session it refuses to resume).
    pub fn try_open(
        &self,
        script: Script,
        vars: &[(&str, &str)],
        mut watch: Option<Watcher>,
    ) -> Result<Worker, String> {
        let (port, requests) = server(script);
        let agent_dir = self.root.join("agent");
        std::fs::write(
            agent_dir.join("models.json"),
            json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}).to_string(),
        )
        .unwrap();
        let python = kernel::env::python(|name| std::env::var(name).ok());
        let mut all: Vec<(String, String)> = vec![
            ("HOME".into(), self.root.display().to_string()),
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            (config::paths::ENV_AGENT_DIR.into(), agent_dir.display().to_string()),
            (
                config::paths::ENV_SERVER_DIR.into(),
                self.root.join("server").display().to_string(),
            ),
            ("ANTHROPIC_API_KEY".into(), "test-key".into()),
            (hindsight::resolve::HINDSIGHT_URL_ENV.into(), "off".into()),
            ("ULTRON_MEMORANTADO_URL".into(), "off".into()),
            (kernel::env::PYTHON_ENV.into(), python),
            (config::env::OFFLINE_ENV.into(), "1".into()),
            ("TZ".into(), "UTC".into()),
        ];
        all.extend(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let env = session_core::RunEnv {
            cwd: self.cwd.clone(),
            home: self.root.clone(),
            var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
            package: config::paths::normalize(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent"),
            ),
        };
        let repo = session_journal::SessionRepo::new(&self.sessions(), session_journal::SystemClock::shared());
        let metadata = repo.list(None).into_iter().next().expect("the session exists");
        let open = session_core::core::OpenOptions {
            env,
            flags: session_core::ModelFlags {
                model: Some("anthropic/claude-sonnet-4-5".into()),
                ..Default::default()
            },
            session_root: self.sessions(),
            metadata,
            mode: "rpc".into(),
            services: session_core::compose::Services::default(),
            auto_name: false,
        };
        let events: Arc<Log<Value>> = Arc::default();
        let sink = events.clone();
        let input: Arc<OnceLock<Sender<Input>>> = Arc::new(OnceLock::new());
        let actor_input = input.clone();
        let (tx, actor, _) = session_core::core::spawn(
            open,
            Box::new(move |event| {
                if let (Some(watch), Some(tx)) = (watch.as_mut(), actor_input.get()) {
                    watch(&event, tx);
                }
                sink.push(event);
            }),
        )?;
        let _ = input.set(tx.clone());
        Ok(Worker {
            tx,
            events,
            requests,
            actor: Some(actor),
        })
    }
}

impl Worker {
    pub fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    pub fn events(&self, kind: &str) -> Vec<Value> {
        self.events.of_kind(kind)
    }

    pub fn wait_event(&self, kind: &str) -> Value {
        self.events.wait_latest(kind)
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.all()
    }

    /// Hand the session over to a successor (the worker's `HandOver`): the run in progress stays open in the
    /// journal for the next worker.
    pub fn hand_over(mut self) {
        let (done, handed) = channel();
        if self.tx.send(Input::HandOver(done)).is_ok() {
            handed.recv_timeout(HANG).expect("the actor hands over");
        }
        if let Some(actor) = self.actor.take() {
            let _ = actor.join();
        }
    }

    /// Close the session (the worker's `Close`).
    pub fn close(mut self) {
        self.close_now();
    }

    fn close_now(&mut self) {
        let Some(actor) = self.actor.take() else { return };
        let (done, closed) = channel();
        if self.tx.send(Input::Close(done)).is_ok() && closed.recv_timeout(HANG).is_ok() {
            let _ = actor.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.close_now();
    }
}
