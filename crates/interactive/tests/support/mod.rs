//! What the app tests share: the options an app under test starts with, and what it sent to its session (the
//! commands on its lane channel, kept in order).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::keybindings::{Platform, native_app_keybindings};
use interactive::lane::{LaneCommand, ModelInfo};
use interactive::qol::QolSettings;
use theme::{ColorMode, Theme, builtin_theme_json, create_theme};
use tui::KeybindingsManager;

pub struct Sent {
    commands: mpsc::Receiver<LaneCommand>,
    seen: RefCell<Vec<LaneCommand>>,
    record: fn(&LaneCommand) -> Option<String>,
}

impl Sent {
    /// The sender an app sends on, and what it sent; `record` names the commands a test follows (others are
    /// sent but not listed by [`Sent::calls`]).
    pub fn new(record: fn(&LaneCommand) -> Option<String>) -> (mpsc::Sender<LaneCommand>, Rc<Sent>) {
        let (tx, commands) = mpsc::channel();
        (
            tx,
            Rc::new(Sent {
                commands,
                seen: RefCell::default(),
                record,
            }),
        )
    }

    /// Every command sent so far, oldest first.
    pub fn commands(&self) -> Vec<LaneCommand> {
        let mut seen = self.seen.borrow_mut();
        seen.extend(self.commands.try_iter());
        seen.clone()
    }

    /// The followed commands sent so far, as `record` names them.
    pub fn calls(&self) -> Vec<String> {
        self.commands().iter().filter_map(self.record).collect()
    }

    /// Forget what was sent so far.
    pub fn clear(&self) {
        self.commands();
        self.seen.borrow_mut().clear();
    }
}

/// The dark built-in theme in truecolor.
pub fn dark_theme() -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap())
}

/// The native TUI's keys on Linux, without user bindings.
pub fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

/// What the tests start an app with unless they say otherwise: the parity build in `/p` on one reasoning model,
/// no splash, history in memory, and `rlm` cells unhighlighted.
pub fn options() -> AppOptions {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    AppOptions {
        theme: dark_theme(),
        keys: keys(),
        version: "1.2.3".into(),
        cwd: "/p".into(),
        home: None,
        server_id: "server".into(),
        models: vec![model.clone()],
        model,
        thinking_level: "medium".into(),
        splash: false,
        history: PromptHistoryStore::new(None, "/p"),
        fd_path: None,
        highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
        qol: QolSettings::parity(),
        bug_report_target: ("radius.pi.dev".into(), false),
    }
}

/// Apply what the app's service threads (clipboard, git) answered until `done`: the answers arrive on those threads'
/// channels, which the app only reads when polled. The bound only guards a hang.
pub fn poll_services_until(app: &mut App, mut done: impl FnMut(&mut App) -> bool) {
    let guard = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !done(app) {
        assert!(std::time::Instant::now() < guard, "a service never answered");
        app.poll_services();
        std::thread::yield_now();
    }
}
