//! The loopback server the authorization server redirects the browser to (`OAuthCallbackServer` of
//! oauth/callback.ts): it listens on a free port unless one is configured, answers the browser with a page, and hands
//! the authorization code of the pending sign-in (matched by its `state`) to the waiting flow.

use super::OAuthError;
use jsrt::sync;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// What the browser's redirect delivered.
#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

/// The browser page after the redirect: `None` is success, else the message and details.
pub type PageRenderer = Arc<dyn Fn(Option<(&str, Option<&str>)>) -> String + Send + Sync>;

type Pending = HashMap<String, Sender<Result<Callback, OAuthError>>>;

pub struct CallbackServer {
    pub redirect_url: String,
    address: std::net::SocketAddr,
    pending: Arc<Mutex<Pending>>,
    closed: Arc<Mutex<bool>>,
    timeout: Duration,
}

impl CallbackServer {
    /// Listens on `host:port` (port 0: a free one). The redirect URL names `redirect_host`.
    pub fn listen(
        host: &str,
        redirect_host: &str,
        port: u16,
        path: &str,
        render: PageRenderer,
    ) -> Result<CallbackServer, OAuthError> {
        let bind_host = host.trim_start_matches('[').trim_end_matches(']');
        let listener = TcpListener::bind((bind_host, port))
            .map_err(|e| OAuthError::Other(format!("listen {host}:{port}: {e}")))?;
        let address = listener.local_addr().map_err(|e| OAuthError::Other(e.to_string()))?;
        let shown_host = if redirect_host.contains(':') && !redirect_host.starts_with('[') {
            format!("[{redirect_host}]")
        } else {
            redirect_host.to_string()
        };
        let server = CallbackServer {
            redirect_url: format!("http://{shown_host}:{}{path}", address.port()),
            address,
            pending: Arc::default(),
            closed: Arc::default(),
            timeout: Duration::from_secs(5 * 60),
        };
        let (pending, closed, path, base) = (
            server.pending.clone(),
            server.closed.clone(),
            path.to_string(),
            server.redirect_url.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if *sync::lock(&closed) {
                    return;
                }
                if let Ok(stream) = stream {
                    handle(stream, &path, &base, &pending, &render);
                }
            }
        });
        Ok(server)
    }

    /// Waits for the callback with `state` from now on, for at most the server's timeout.
    pub fn wait_for_callback(&self, state: &str) -> Result<PendingCallback, OAuthError> {
        let mut pending = sync::lock(&self.pending);
        if pending.contains_key(state) {
            return Err(OAuthError::Other("OAuth state is already pending".into()));
        }
        let (tx, rx) = channel();
        pending.insert(state.to_string(), tx);
        Ok(PendingCallback {
            rx,
            pending: self.pending.clone(),
            state: state.to_string(),
            deadline: Instant::now() + self.timeout,
        })
    }

    pub fn close(&self) {
        *sync::lock(&self.closed) = true;
        for (_, tx) in sync::lock(&self.pending).drain() {
            let _ = tx.send(Err(OAuthError::Other("OAuth callback server closed".into())));
        }
        // Wakes the accept loop so it sees the close.
        let _ = TcpStream::connect(self.address);
    }
}

/// A sign-in waiting for its callback ([`CallbackServer::wait_for_callback`]).
pub struct PendingCallback {
    rx: Receiver<Result<Callback, OAuthError>>,
    pending: Arc<Mutex<Pending>>,
    state: String,
    deadline: Instant,
}

impl PendingCallback {
    /// Blocks until the callback came (its code, or why not), the server closed, or the timeout passed.
    pub fn recv(&self) -> Result<Callback, OAuthError> {
        let closed = || OAuthError::Other("OAuth callback server closed".into());
        match self
            .rx
            .recv_timeout(self.deadline.saturating_duration_since(Instant::now()))
        {
            Ok(outcome) => outcome,
            Err(RecvTimeoutError::Disconnected) => Err(closed()),
            Err(RecvTimeoutError::Timeout) => {
                if sync::lock(&self.pending).remove(&self.state).is_some() {
                    return Err(OAuthError::Other("OAuth callback timed out".into()));
                }
                // The callback took the state just now.
                self.rx.recv().unwrap_or_else(|_| Err(closed()))
            }
        }
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        self.close();
    }
}

fn reply(stream: &mut TcpStream, status: u16, render: &PageRenderer, page: Option<(&str, Option<&str>)>) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = render(page);
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\ncontent-type: text/html; charset=utf-8\r\ncache-control: no-store\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn handle(mut stream: TcpStream, path: &str, base: &str, pending: &Mutex<Pending>, render: &PageRenderer) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(clone) => clone,
        Err(_) => return,
    });
    if reader.read_line(&mut line).is_err() || line.is_empty() {
        return;
    }
    // The rest of the request head.
    let mut header = String::new();
    while reader.read_line(&mut header).is_ok_and(|n| n > 0) && header.trim() != "" {
        header.clear();
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    let Ok(url) = Url::parse(base).and_then(|b| b.join(target)) else {
        return reply(&mut stream, 404, render, Some(("Not found", None)));
    };
    if url.path() != path {
        return reply(&mut stream, 404, render, Some(("Not found", None)));
    }
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let state = query.get("state").filter(|s| !s.is_empty());
    let sender = state.and_then(|s| sync::lock(pending).remove(s));
    let (Some(state), Some(sender)) = (state, sender) else {
        return reply(&mut stream, 400, render, Some(("Invalid or expired OAuth state", None)));
    };
    if let Some(error) = query.get("error") {
        let description = query.get("error_description").unwrap_or(error).clone();
        let _ = sender.send(Err(OAuthError::Other(description.clone())));
        return reply(
            &mut stream,
            200,
            render,
            Some(("Authorization failed. You may close this window.", Some(&description))),
        );
    }
    let Some(code) = query.get("code").filter(|c| !c.is_empty()) else {
        let _ = sender.send(Err(OAuthError::Other(
            "OAuth callback did not include an authorization code".into(),
        )));
        return reply(&mut stream, 400, render, Some(("Missing authorization code", None)));
    };
    let _ = sender.send(Ok(Callback {
        code: code.clone(),
        state: state.clone(),
        iss: query.get("iss").filter(|i| !i.is_empty()).cloned(),
    }));
    reply(&mut stream, 200, render, None);
}
