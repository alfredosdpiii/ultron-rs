//! The per-session MCP inventory bridge: a loopback streamable-HTTP MCP server that `agy` connects to as the
//! session's only extra server. It lists the request's tools and executes nothing: a `tools/call` is captured for
//! Ultron's loop and its HTTP response is held open, so the agent can neither see a result nor run past the call.
//! Requests without the session's bearer token are refused.

use serde_json::{Value, json};
use std::collections::HashSet;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const PATH: &str = "/mcp";
/// A request body larger than this is refused; tool arguments are code cells, never this big.
const MAX_BODY: usize = 16 << 20;

/// One tool call the agent made through the bridge.
#[derive(Clone, Debug, PartialEq)]
pub struct Captured {
    pub name: String,
    pub arguments: Value,
}

/// A running bridge; dropping it closes the listener and every connection.
pub struct Bridge {
    pub url: String,
    pub token: String,
    pub calls: mpsc::UnboundedReceiver<Captured>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The MCP `tools/list` entries of `tools`.
pub fn manifest(tools: &[crate::types::Tool]) -> Value {
    tools
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "inputSchema": t.parameters}))
        .collect()
}

impl Bridge {
    pub async fn start(manifest: Value) -> std::io::Result<Bridge> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let url = format!("http://{}{PATH}", listener.local_addr()?);
        let token = jsrt::random::random_hex(16);
        let (sender, calls) = mpsc::unbounded_channel();
        let shared = std::sync::Arc::new(Inventory {
            names: manifest
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["name"].as_str().map(str::to_string))
                .collect(),
            manifest,
            authorization: format!("Bearer {token}"),
            sender,
        });
        let task = tokio::spawn(async move {
            // Connections live in the set, so aborting this task drops every held `tools/call` with it.
            let mut connections = JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                connections.spawn(serve(stream, shared.clone()));
                while connections.try_join_next().is_some() {}
            }
        });
        Ok(Bridge {
            url,
            token,
            calls,
            task,
        })
    }
}

struct Inventory {
    manifest: Value,
    names: HashSet<String>,
    authorization: String,
    sender: mpsc::UnboundedSender<Captured>,
}

struct Request {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
}

enum Reply {
    Json(Value),
    Status(u16),
    Hold,
}

async fn read_request(reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Option<Request> {
    let mut line = String::new();
    reader.read_line(&mut line).await.ok().filter(|n| *n > 0)?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut length = 0usize;
    let mut authorization = None;
    loop {
        line.clear();
        reader.read_line(&mut line).await.ok().filter(|n| *n > 0)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "authorization" => authorization = Some(value.trim().to_string()),
            "transfer-encoding" => return None,
            _ => {}
        }
    }
    if length > MAX_BODY {
        return None;
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.ok()?;
    Some(Request {
        method,
        path,
        authorization,
        body,
    })
}

fn rpc_result(id: &Value, result: Value) -> Reply {
    Reply::Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn answer(inventory: &Inventory, request: &Request) -> Reply {
    if request.method != "POST" || request.path != PATH {
        return Reply::Status(404);
    }
    if request.authorization.as_deref() != Some(inventory.authorization.as_str()) {
        return Reply::Status(401);
    }
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return Reply::Status(400);
    };
    let Some(id) = message.get("id") else {
        return Reply::Status(202);
    };
    let params = &message["params"];
    match message["method"].as_str() {
        Some("initialize") => rpc_result(
            id,
            json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "ultron", "version": "1"},
            }),
        ),
        Some("ping") => rpc_result(id, json!({})),
        Some("tools/list") => rpc_result(id, json!({"tools": inventory.manifest})),
        Some("tools/call") => {
            let name = params["name"].as_str().unwrap_or_default();
            if !inventory.names.contains(name) {
                return rpc_result(
                    id,
                    json!({"isError": true, "content": [{"type": "text", "text": format!("Unknown tool: {name}")}]}),
                );
            }
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let _ = inventory.sender.send(Captured {
                name: name.to_string(),
                arguments,
            });
            Reply::Hold
        }
        _ => Reply::Json(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})),
    }
}

async fn serve(stream: TcpStream, inventory: std::sync::Arc<Inventory>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    while let Some(request) = read_request(&mut reader).await {
        let (status, body) = match answer(&inventory, &request) {
            Reply::Json(value) => (200, value.to_string()),
            Reply::Status(status) => (status, String::new()),
            Reply::Hold => return std::future::pending().await,
        };
        let head = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            if status == 200 { "OK" } else { "-" },
            body.len()
        );
        if write.write_all(head.as_bytes()).await.is_err() || write.write_all(body.as_bytes()).await.is_err() {
            return;
        }
    }
}
