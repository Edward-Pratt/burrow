//! `burrow mcp serve`: an MCP server on stdin and stdout, so an AI
//! assistant such as Claude can use Burrow.
//!
//! It connects to the daemon in the `mcp` role, so the daemon decides what
//! each tool may do from the grants the user gave; this side only turns
//! tools into commands. It speaks both MCP eras: the `initialize` handshake
//! of 2025 versions, and the per-request metadata of 2026-07-28, which
//! clients find with `server/discover`.

use std::collections::HashMap;
use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::{Command, Node, NodeId, Response, SessionId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Result, nodes::find};

mod calls;
mod monitor;
mod sessions;
mod tools;

use tools::tools;

/// Protocol versions this server speaks, newest first. The first is
/// stateless; the others start with `initialize`.
const VERSIONS: [&str; 3] = ["2026-07-28", "2025-11-25", "2025-06-18"];

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

const INSTRUCTIONS: &str = "Burrow manages the user's SSH hosts, tunnels and Postgres \
databases. Call list_nodes first: it shows every node and what you may do on each. \
You can only act where the user granted it; ask them to grant more in Burrow \
(right-click a node, \"Claude access…\") or with `burrow mcp grant`. Without \
sql-write, queries run read-only.";

pub async fn serve(client: Client) -> Result<ExitCode> {
    let mut bridge = Bridge {
        client,
        sessions: HashMap::new(),
    };
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Notifications, such as notifications/initialized, need no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message["method"].as_str().unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let reply = match bridge.handle(method, &params).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message, data)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message, "data": data},
            }),
        };
        let mut line = reply.to_string();
        line.push('\n');
        stdout.write_all(line.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(ExitCode::SUCCESS)
}

type Failure = (i64, String, Value);

/// Which kind of session a tool needs on a node.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Database,
    Files,
    Monitor,
}

struct Bridge {
    client: Client,
    /// Sessions opened for tools, reused by later calls on the same node.
    sessions: HashMap<(NodeId, Kind), SessionId>,
}

impl Bridge {
    async fn handle(&mut self, method: &str, params: &Value) -> Result<Value, Failure> {
        // A 2026 request names its version on every call.
        let requested = params["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str();
        if let Some(version) = requested
            && !VERSIONS.contains(&version)
        {
            return Err((
                UNSUPPORTED_VERSION,
                "Unsupported protocol version".into(),
                json!({"supported": VERSIONS, "requested": version}),
            ));
        }
        let server_info = json!({"name": "burrow", "version": burrow_api::BUILD});
        let mut result = match method {
            "server/discover" => json!({
                "supportedVersions": VERSIONS,
                "capabilities": {"tools": {}},
                "instructions": INSTRUCTIONS,
            }),
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or_default();
                // The client's version when it is one of ours, else the
                // newest one with this handshake.
                let version = VERSIONS[1..]
                    .iter()
                    .find(|v| **v == asked)
                    .unwrap_or(&VERSIONS[1]);
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": server_info,
                    "instructions": INSTRUCTIONS,
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": tools()}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                // A tool that fails tells the assistant why, as a result:
                // protocol errors are for calls that make no sense.
                match self.call(name, &arguments).await {
                    Ok(text) => {
                        json!({"content": [{"type": "text", "text": text}], "isError": false})
                    }
                    Err(Called::Invalid(message)) => {
                        return Err((INVALID_PARAMS, message, Value::Null));
                    }
                    Err(Called::Failed(message)) => {
                        json!({"content": [{"type": "text", "text": message}], "isError": true})
                    }
                }
            }
            _ => {
                return Err((
                    METHOD_NOT_FOUND,
                    format!("unknown method {method:?}"),
                    Value::Null,
                ));
            }
        };
        if requested.is_some() {
            result["resultType"] = json!("complete");
            result["_meta"] = json!({"io.modelcontextprotocol/serverInfo": server_info});
        }
        Ok(result)
    }

    async fn node(&self, arg: &str) -> Result<Node, Called> {
        find(&self.client, arg)
            .await
            .map_err(|e| Called::Failed(e.to_string()))
    }

    async fn execute(&self, command: Command) -> Result<Response, Called> {
        self.client
            .execute(command)
            .await
            .map_err(|e| Called::Failed(e.to_string()))
    }
}

enum Called {
    /// The call itself is wrong: a missing argument or an unknown tool.
    Invalid(String),
    /// The tool ran and failed, or Burrow refused it.
    Failed(String),
}
fn pretty(response: &Response) -> String {
    serde_json::to_string_pretty(response).unwrap_or_default()
}
