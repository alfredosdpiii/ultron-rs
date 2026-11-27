//! A run's [`SessionLink`] (crates/client) to the session its flags select: this process's own server generation for
//! the instance (a client of the durable instance uses its detached server, crate::durable_client), the session
//! selected (created, forked or opened) and attached, then supervised.

use crate::Env;
use crate::args::Args;
use crate::print_mode::{Target, open_route, server_options, server_start, session_options};
use client::session::{SessionLink, WatchSink};
use serde_json::json;

fn rpc_trace(mode: &str, env: &Env, label: &str) {
    if mode == "rpc" {
        crate::print_mode::trace_startup(env, label);
    }
}

fn named_options(mut options: serde_json::Value, name: Option<&str>) -> serde_json::Value {
    if let Some(name) = name {
        options["name"] = json!(name);
    }
    options
}

fn select_session(link: &mut SessionLink, target: &Target, name: Option<&str>) -> Result<String, String> {
    match target {
        Target::Open(metadata) => match name {
            Some(name) => link.rename(&metadata.id, name).map(|()| metadata.id.clone()),
            None => Ok(metadata.id.clone()),
        },
        Target::New { id } => {
            let base = id.as_ref().map_or_else(|| json!({}), |id| json!({"id": id}));
            link.create(named_options(base, name))
        }
        Target::Fork(metadata) => link.create(named_options(json!({"forkFromSessionId": metadata.id}), name)),
        Target::Ephemeral => link.create(named_options(json!({}), name)),
    }
}

/// Start the server, select `target`'s session and attach to it; transcript events go to `events`. Closing the link
/// never stops a server it did not start.
pub fn open(parsed: &Args, env: &Env, target: &Target, mode: &str, events: WatchSink) -> Result<SessionLink, String> {
    let options = server_options(parsed, env, mode)?;
    let start = server_start(env, &options);
    let started = open_route(env, &start)?;
    rpc_trace(mode, env, "rpc.open");
    let mut link = SessionLink::connect(start, started, session_options(env, &options), events)?;
    rpc_trace(mode, env, "rpc.services-ready");
    let selected = select_session(&mut link, target, parsed.name.as_deref());
    let attached = selected.and_then(|id| {
        rpc_trace(mode, env, "rpc.session-selected");
        link.attach(&id)
    });
    if let Err(error) = attached {
        link.close(false);
        return Err(error);
    }
    rpc_trace(mode, env, "rpc.attached");
    link.supervise();
    Ok(link)
}

/// The worker's open `/mcp` dialogs and MCP notices, polled (`extension-ui` `poll`): each poll returns the requests
/// after the last one seen. A different session starts again from its open dialogs.
#[derive(Default)]
pub struct ExtensionUiPoll {
    session: Option<String>,
    cursor: Option<u64>,
}

impl ExtensionUiPoll {
    /// The new requests of `session`. Short polls: a poll held open in the worker would keep it from retiring after
    /// this client is gone.
    pub fn poll(&mut self, session: &str, connection: &client::runtime::Connection) -> Vec<serde_json::Value> {
        if self.session.as_deref() != Some(session) {
            self.session = Some(session.to_string());
            self.cursor = None;
        }
        let polled = connection
            .session_target()
            .and_then(|target| connection.call(target, protocol::names::EXTENSION_UI, "poll", json!([self.cursor, 0])));
        let Ok(Some(mut result)) = polled else {
            self.cursor = None;
            return Vec::new();
        };
        self.cursor = result["cursor"].as_u64();
        let requests = match result["requests"].take() {
            serde_json::Value::Array(items) => items,
            _ => Vec::new(),
        };
        requests.into_iter().map(|mut item| item["request"].take()).collect()
    }
}
