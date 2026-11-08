//! A run's [`SessionLink`] (crates/client) to the session its flags select: this process's own server generation for
//! the instance (a client of the durable instance uses its detached server, crate::durable_client), the session
//! selected (created, forked or opened) and attached, then supervised.

use crate::Env;
use crate::args::Args;
use crate::print_mode::{Target, open_route, server_options, server_start, session_options};
use client::session::{SessionLink, WatchSink};
use serde_json::json;

/// Start the server, select `target`'s session and attach to it; transcript events go to `events`. Closing the link
/// never stops a server it did not start.
pub fn open(parsed: &Args, env: &Env, target: &Target, mode: &str, events: WatchSink) -> Result<SessionLink, String> {
    let options = server_options(parsed, env, mode)?;
    let start = server_start(env, &options);
    let started = open_route(env, &start)?;
    // rpc-native.ts's milestones: the client runtime opens after the server started, its services are ready once
    // connected, then the session is selected and attached.
    let rpc_trace = |label: &str| {
        if mode == "rpc" {
            crate::print_mode::trace_startup(env, label);
        }
    };
    rpc_trace("rpc.open");
    let mut link = SessionLink::connect(start, started, session_options(env, &options), events)?;
    rpc_trace("rpc.services-ready");
    let name = parsed.name.as_deref();
    let named = |mut options: serde_json::Value| {
        if let Some(name) = name {
            options["name"] = json!(name);
        }
        options
    };
    let selected = match target {
        Target::Open(metadata) => match name {
            Some(name) => link.rename(&metadata.id, name).map(|()| metadata.id.clone()),
            None => Ok(metadata.id.clone()),
        },
        Target::New { id } => link.create(named(match id {
            Some(id) => json!({"id": id}),
            None => json!({}),
        })),
        Target::Fork(metadata) => link.create(named(json!({"forkFromSessionId": metadata.id}))),
        // `--no-session --name x`: the session is removed when the run ends, but named while it lasts.
        Target::Ephemeral => link.create(named(json!({}))),
    };
    let attached = selected.and_then(|id| {
        rpc_trace("rpc.session-selected");
        link.attach(&id)
    });
    if let Err(error) = attached {
        link.close(false);
        return Err(error);
    }
    rpc_trace("rpc.attached");
    link.supervise();
    Ok(link)
}
