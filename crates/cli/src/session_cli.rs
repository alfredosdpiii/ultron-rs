//! `ultron session send <session id> <message|->` (Rust-only): hand a message to a session as the user's prompt,
//! through the running server that holds it (daemon::session_inbox). An idle session runs it at once; a session in a
//! run queues it as a follow-up. `-` reads the message from stdin. Answer me with HTML's Send button uses it.

use crate::Env;
use daemon::instance::{self, Kind};
use protocol::RpcTarget;
use serde_json::json;
use std::io::{Read, Write};

const USAGE: &str = "Usage: ultron session send <session id> <message|->";

fn io(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// `ultron session ...`; `None` when `args` is not a session command (a prompt that starts with "session").
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> Option<Result<i32, String>> {
    match args {
        [send, id, message] if send == "send" => Some(send_message(env, id, message, out)),
        [send, ..] if send == "send" => Some(writeln!(err, "{USAGE}").map(|()| 2).map_err(io)),
        _ => None,
    }
}

fn send_message(env: &Env, id: &str, message: &str, out: &mut dyn Write) -> Result<i32, String> {
    let message = if message == "-" {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text).map_err(io)?;
        text
    } else {
        message.to_string()
    };
    if message.trim().is_empty() {
        return Err("The message is empty".into());
    }
    let current = crate::instance(env, false);
    let explicit = (current.kind == Kind::Explicit).then_some(current.dir);
    let server = instance::running(&env.home, explicit.as_deref())
        .into_iter()
        .find(|s| matches!(session_journal::catalog::find(&s.session_dir, id), Ok(Some(_))))
        .ok_or_else(|| {
            format!("No running Ultron server holds session {id}; resume it (ultron --session {id}) and send again")
        })?;
    let socket = instance::socket_path(&server.dir, &server.server_id);
    let connection = client::runtime::Connection::connect(&socket, &server.server_id).map_err(io)?;
    let answer = connection.call(
        RpcTarget::Server {
            server_id: server.server_id.clone(),
        },
        protocol::names::SESSION_INBOX,
        "send",
        json!([id, message]),
    );
    connection.disconnect();
    answer.map_err(io)?;
    writeln!(out, "Sent to session {id}").map_err(io)?;
    Ok(0)
}
