//! Where each client connection of a server generation is routed (crate::routed `Router`): attached to a session, or
//! waiting to be re-attached to the session whose worker is restarting. One entry per connection, so a connection is
//! never both attached to one session and waiting for another: attaching replaces the wait.

use std::collections::HashMap;

/// One connection's route; `A` is the attachment the router holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route<A> {
    Attached {
        session: String,
        attachment: A,
    },
    /// The session's worker ended and is being restarted: the connection is re-attached to it when it is back.
    Reconnecting {
        session: String,
    },
}

#[derive(Debug)]
pub struct Routes<A> {
    routes: HashMap<u64, Route<A>>,
}

impl<A> Default for Routes<A> {
    fn default() -> Self {
        Routes { routes: HashMap::new() }
    }
}

impl<A> Routes<A> {
    pub fn get(&self, conn: u64) -> Option<&Route<A>> {
        self.routes.get(&conn)
    }

    /// `conn`'s attachment to `session`.
    pub fn attached(&self, conn: u64, session: &str) -> Option<&A> {
        match self.routes.get(&conn) {
            Some(Route::Attached { session: s, attachment }) if s == session => Some(attachment),
            _ => None,
        }
    }

    /// Whether `conn` waits to be re-attached to `session`.
    pub fn reconnecting(&self, conn: u64, session: &str) -> bool {
        matches!(self.routes.get(&conn), Some(Route::Reconnecting { session: s }) if s == session)
    }

    /// `conn` is attaching to `session`: its current attachment is taken out (the caller releases it), and a wait for
    /// another session ends (the client left it). A wait for `session` itself lasts until the attach succeeds.
    pub fn begin_attach(&mut self, conn: u64, session: &str) -> Option<A> {
        match self.routes.get(&conn) {
            Some(Route::Attached { .. }) => self.take(conn),
            Some(Route::Reconnecting { session: s }) if s != session => {
                self.routes.remove(&conn);
                None
            }
            _ => None,
        }
    }

    /// `conn` is attached to `session` (replacing any wait).
    pub fn attach(&mut self, conn: u64, session: &str, attachment: A) {
        self.routes.insert(
            conn,
            Route::Attached {
                session: session.to_string(),
                attachment,
            },
        );
    }

    /// `session`'s worker ended: `conn`'s attachment to it is taken out; with `restarting`, `conn` waits to be
    /// re-attached to it.
    pub fn invalidate(&mut self, conn: u64, session: &str, restarting: bool) -> Option<A> {
        let attachment = self
            .attached(conn, session)
            .is_some()
            .then(|| self.take(conn))
            .flatten();
        if attachment.is_some() && restarting {
            self.routes.insert(
                conn,
                Route::Reconnecting {
                    session: session.to_string(),
                },
            );
        }
        attachment
    }

    /// The re-attach of `conn` to `session` is over (done or given up): its wait ends, unless it no longer waits for
    /// `session`.
    pub fn end_reconnect(&mut self, conn: u64, session: &str) {
        if self.reconnecting(conn, session) {
            self.routes.remove(&conn);
        }
    }

    /// `conn` detached or went away: its route ends; its attachment, if any, is returned for release.
    pub fn remove(&mut self, conn: u64) -> Option<A> {
        self.take(conn)
    }

    /// Every attachment (the generation closes); waits end too.
    pub fn drain(&mut self) -> Vec<(u64, A)> {
        self.routes
            .drain()
            .filter_map(|(conn, route)| match route {
                Route::Attached { attachment, .. } => Some((conn, attachment)),
                Route::Reconnecting { .. } => None,
            })
            .collect()
    }

    fn take(&mut self, conn: u64) -> Option<A> {
        match self.routes.remove(&conn)? {
            Route::Attached { attachment, .. } => Some(attachment),
            Route::Reconnecting { .. } => None,
        }
    }
}
