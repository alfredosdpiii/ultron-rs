//! What an app under test sent to its session: the commands on its lane channel, kept in order.
#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;

use interactive::lane::LaneCommand;

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
