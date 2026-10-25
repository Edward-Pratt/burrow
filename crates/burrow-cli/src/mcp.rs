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
use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, Node, NodeId, Response, SessionId, SessionState};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{Result, find};

/// Protocol versions this server speaks, newest first. The first is
/// stateless; the others start with `initialize`.
const VERSIONS: [&str; 3] = ["2026-07-28", "2025-11-25", "2025-06-18"];

/// JSON-RPC error codes.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_VERSION: i64 = -32022;

/// How long a database or file session may take to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the monitor tool waits for a new monitor's first sample.
const FIRST_SAMPLE: Duration = Duration::from_secs(5);

/// Rows a query keeps when the assistant doesn't say. Results go into its
/// context, so fewer than the window's default.
const DEFAULT_ROWS: u32 = 200;

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
        let server_info = json!({"name": "burrow", "version": burrow_core::ipc::BUILD});
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

    async fn call(&mut self, tool: &str, args: &Value) -> Result<String, Called> {
        let text = |key: &str| {
            args[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| Called::Invalid(format!("{key} is missing")))
        };
        let number = |key: &str| args[key].as_u64();
        let response = match tool {
            "list_nodes" => return self.list_nodes().await,
            "run_command" => {
                let node = self.node(&text("node")?).await?;
                let timeout = number("timeout_seconds").map(|t| t.min(3600) as u32);
                let command = Command::Exec {
                    node: node.id,
                    command: text("command")?,
                    timeout,
                };
                self.execute(command).await?
            }
            "query" => {
                let session = self.session(&text("node")?, Kind::Database).await?;
                let limit = number("max_rows").map_or(DEFAULT_ROWS, |n| n.min(10_000) as u32);
                let command = Command::RunQuery {
                    session,
                    sql: text("sql")?,
                    limit: Some(limit),
                };
                self.execute(command).await?
            }
            "list_schema" => {
                let session = self.session(&text("node")?, Kind::Database).await?;
                self.execute(Command::ListSchema { session }).await?
            }
            "list_files" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let path = args["path"].as_str().unwrap_or_default().to_string();
                self.execute(Command::ListDir { session, path }).await?
            }
            "read_file" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let command = Command::ReadFile {
                    session,
                    path: text("path")?,
                    limit: number("max_bytes"),
                };
                match self.execute(command).await? {
                    Response::File { data, size } => return Ok(file_text(&data, size)),
                    other => other,
                }
            }
            "write_file" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let command = Command::WriteFile {
                    session,
                    path: text("path")?,
                    data: text("content")?.into_bytes(),
                    overwrite: args["overwrite"].as_bool().unwrap_or(false),
                };
                self.execute(command).await?
            }
            "make_dir" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let path = text("path")?;
                self.execute(Command::MakeDir { session, path }).await?
            }
            "rename_path" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let (from, to) = (text("from")?, text("to")?);
                self.execute(Command::RenamePath { session, from, to })
                    .await?
            }
            "remove_path" => {
                let session = self.session(&text("node")?, Kind::Files).await?;
                let path = text("path")?;
                self.execute(Command::RemovePath { session, path }).await?
            }
            "start_tunnel" => {
                let node = self.node(&text("node")?).await?;
                let session = match self.execute(Command::StartTunnel { node: node.id }).await? {
                    Response::Session { session } => session,
                    other => return Ok(pretty(&other)),
                };
                self.wait(session).await?;
                return Ok(format!("{} is up", node.name));
            }
            "stop_tunnel" => {
                let node = self.node(&text("node")?).await?;
                let running = self
                    .sessions_list()
                    .await?
                    .into_iter()
                    .find(|s| s.node == node.id && s.kind == burrow_core::SessionKind::Tunnel);
                let Some(running) = running else {
                    return Ok(format!("{} was not running", node.name));
                };
                let command = Command::CloseSession {
                    session: running.session,
                };
                self.execute(command).await?
            }
            "monitor" => {
                let session = self.session(&text("node")?, Kind::Monitor).await?;
                let all_units = args["units"].as_str() == Some("all");
                return self.monitor(session, all_units).await;
            }
            _ => return Err(Called::Invalid(format!("no tool is called {tool:?}"))),
        };
        Ok(pretty(&response))
    }

    /// Every node, with what the assistant may do on it.
    async fn list_nodes(&self) -> Result<String, Called> {
        let nodes = match self.execute(Command::ListNodes).await? {
            Response::Nodes { nodes } => nodes,
            other => return Ok(pretty(&other)),
        };
        let grants = match self.execute(Command::ListGrants).await? {
            Response::Grants { grants } => grants,
            _ => Vec::new(),
        };
        let path = |node: &Node| {
            let mut names = vec![node.name.clone()];
            let mut parent = node.parent;
            while let Some(id) = parent {
                let Some(up) = nodes.iter().find(|n| n.id == id) else {
                    break;
                };
                names.push(up.name.clone());
                parent = up.parent;
            }
            names.reverse();
            names.join("/")
        };
        let listed: Vec<Value> = nodes
            .iter()
            .filter(|n| !n.is_folder())
            .map(|node| {
                let grant = grants.iter().find(|g| g.node == node.id && !g.stale);
                let allowed: Vec<String> = grant
                    .map(|g| g.capabilities.iter().map(|c| c.to_string()).collect())
                    .unwrap_or_default();
                json!({
                    "id": node.id,
                    "name": node.name,
                    "path": path(node),
                    "kind": node.kind,
                    "settings": node.props,
                    "allowed": allowed,
                })
            })
            .collect();
        Ok(serde_json::to_string_pretty(&listed).unwrap_or_default())
    }

    /// The latest stats and a summary of the history, rather than every
    /// sample, which would fill the assistant's context.
    async fn monitor(&self, session: SessionId, all_units: bool) -> Result<String, Called> {
        // A monitor that just started has no sample yet.
        let deadline = tokio::time::Instant::now() + FIRST_SAMPLE;
        let history = loop {
            let history = match self.execute(Command::MonitorHistory { session }).await? {
                Response::Monitor { history } => history,
                other => return Ok(pretty(&other)),
            };
            if !history.samples.is_empty() || tokio::time::Instant::now() >= deadline {
                break history;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let mut summary = monitor_summary(&history, all_units);
        // A host monitored in the background has alerts too.
        let running = self.sessions_list().await?;
        let info = running.iter().find(|s| s.session == session);
        if let Some(info) = info.filter(|info| info.background) {
            let alerts = match self.execute(Command::ListAlerts).await? {
                Response::Alerts { alerts } => alerts,
                _ => Vec::new(),
            };
            let alerts: Vec<_> = alerts.into_iter().filter(|a| a.node == info.node).collect();
            summary["background"] = json!(true);
            summary["alerts"] = json!(alerts);
        }
        Ok(serde_json::to_string_pretty(&summary).unwrap_or_default())
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

    async fn sessions_list(&self) -> Result<Vec<burrow_core::SessionInfo>, Called> {
        match self.execute(Command::ListSessions).await? {
            Response::Sessions { sessions } => Ok(sessions),
            _ => Ok(Vec::new()),
        }
    }

    /// A connected session of `kind` on the node, opened on first use.
    async fn session(&mut self, arg: &str, kind: Kind) -> Result<SessionId, Called> {
        let node = self.node(arg).await?;
        if let Some(&session) = self.sessions.get(&(node.id, kind)) {
            let running = self.sessions_list().await?;
            if running
                .iter()
                .any(|s| s.session == session && s.state == SessionState::Connected)
            {
                return Ok(session);
            }
            self.sessions.remove(&(node.id, kind));
        }
        let command = match kind {
            Kind::Database => Command::OpenDatabase { node: node.id },
            Kind::Files => Command::OpenFiles { node: node.id },
            Kind::Monitor => Command::OpenMonitor { node: node.id },
        };
        let session = match self.execute(command).await? {
            Response::Session { session } => session,
            other => return Err(Called::Failed(pretty(&other))),
        };
        self.wait(session).await?;
        self.sessions.insert((node.id, kind), session);
        Ok(session)
    }

    /// Waits for a session to connect. The bridge hears no session events,
    /// so it asks.
    async fn wait(&self, session: SessionId) -> Result<(), Called> {
        let waited = tokio::time::timeout(CONNECT_TIMEOUT, async {
            loop {
                let running = self.sessions_list().await?;
                match running
                    .iter()
                    .find(|s| s.session == session)
                    .map(|s| &s.state)
                {
                    Some(SessionState::Connected) => return Ok(()),
                    Some(SessionState::Connecting { .. }) => {}
                    _ => {
                        return Err(Called::Failed(
                            "it closed while connecting; open it in Burrow to see why".into(),
                        ));
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        match waited.await {
            Ok(result) => result,
            Err(_) => {
                let _ = self.execute(Command::CloseSession { session }).await;
                Err(Called::Failed(
                    "it did not connect in time. It may be waiting for a password, a \
                     passphrase or a host key that only the user can give: ask them to \
                     connect to it once in Burrow."
                        .into(),
                ))
            }
        }
    }
}

enum Called {
    /// The call itself is wrong: a missing argument or an unknown tool.
    Invalid(String),
    /// The tool ran and failed, or Burrow refused it.
    Failed(String),
}

/// The latest sample, the lowest, mean and highest CPU and memory use
/// over the history, and the services: the failed ones, or all of them.
fn monitor_summary(history: &burrow_core::MonitorHistory, all_units: bool) -> Value {
    let samples = &history.samples;
    let spread = |value: &dyn Fn(&burrow_core::monitor::Sample) -> f64| {
        let values: Vec<f64> = samples.iter().map(value).collect();
        if values.is_empty() {
            return Value::Null;
        }
        let mean = values.iter().sum::<f64>() / values.len() as f64;
        let low = values.iter().copied().fold(f64::INFINITY, f64::min);
        let high = values.iter().copied().fold(0.0, f64::max);
        let round = |v: f64| (v * 10.0).round() / 10.0;
        json!({"min": round(low), "mean": round(mean), "max": round(high)})
    };
    let memory_used = |s: &burrow_core::monitor::Sample| {
        let total = s.memory.total.max(1) as f64;
        (total - s.memory.available as f64) * 100.0 / total
    };
    let minutes = match (samples.first(), samples.last()) {
        (Some(first), Some(last)) => last.time.saturating_sub(first.time) as f64 / 60_000.0,
        _ => 0.0,
    };
    let units = history.units.as_ref().map(|units| {
        let failed: Vec<_> = units.iter().filter(|u| u.active == "failed").collect();
        if all_units {
            json!({"count": units.len(), "failed": failed.len(), "units": units})
        } else {
            json!({"count": units.len(), "failed": failed})
        }
    });
    json!({
        "latest": samples.last(),
        "history": {
            "samples": samples.len(),
            "minutes": (minutes * 10.0).round() / 10.0,
            "cpu_percent": spread(&|s| s.cpu.percent),
            "memory_used_percent": spread(&memory_used),
        },
        "systemd": history.systemd,
        "services": units,
    })
}

fn pretty(response: &Response) -> String {
    serde_json::to_string_pretty(response).unwrap_or_default()
}

/// A file's start as text when it is UTF-8, otherwise as base64.
fn file_text(data: &[u8], size: u64) -> String {
    let shown = data.len() as u64;
    let note = if shown < size {
        format!("(first {shown} of {size} bytes)\n")
    } else {
        String::new()
    };
    match std::str::from_utf8(data) {
        Ok(text) => format!("{note}{text}"),
        Err(_) => format!(
            "{note}(not UTF-8, so in base64)\n{}",
            data_encoding::BASE64.encode(data)
        ),
    }
}

fn tools() -> Value {
    let node = json!({"type": "string", "description": "The node's name or id, from list_nodes"});
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object", "properties": properties, "required": required},
        })
    };
    json!([
        tool(
            "list_nodes",
            "List the user's SSH hosts, tunnels and databases, with their settings and what you may do on each (`allowed`).",
            json!({}),
            &[],
        ),
        tool(
            "run_command",
            "Run a shell command on an SSH host, as `ssh host command` does, and return its exit status, stdout and stderr. Needs exec.",
            json!({
                "node": node,
                "command": {"type": "string"},
                "timeout_seconds": {"type": "integer", "description": "Default 60, at most 3600"},
            }),
            &["node", "command"],
        ),
        tool(
            "query",
            "Run SQL on a Postgres node. Needs sql-read, which runs one statement read-only; sql-write allows writing.",
            json!({
                "node": node,
                "sql": {"type": "string"},
                "max_rows": {"type": "integer", "description": "Rows kept per statement, default 200"},
            }),
            &["node", "sql"],
        ),
        tool(
            "list_schema",
            "List a Postgres database's schemas, tables and columns. Needs sql-read.",
            json!({"node": node}),
            &["node"],
        ),
        tool(
            "list_files",
            "List a folder on an SSH host. Needs files-read.",
            json!({"node": node, "path": {"type": "string", "description": "Empty for the home folder"}}),
            &["node"],
        ),
        tool(
            "read_file",
            "Read a file on an SSH host. Needs files-read.",
            json!({
                "node": node,
                "path": {"type": "string"},
                "max_bytes": {"type": "integer", "description": "Default 1 MiB"},
            }),
            &["node", "path"],
        ),
        tool(
            "write_file",
            "Write a text file on an SSH host. Needs files-write.",
            json!({
                "node": node,
                "path": {"type": "string"},
                "content": {"type": "string"},
                "overwrite": {"type": "boolean", "description": "Replace an existing file"},
            }),
            &["node", "path", "content"],
        ),
        tool(
            "make_dir",
            "Create a folder on an SSH host. Needs files-write.",
            json!({"node": node, "path": {"type": "string"}}),
            &["node", "path"],
        ),
        tool(
            "rename_path",
            "Rename or move a file or folder on an SSH host. Needs files-write.",
            json!({"node": node, "from": {"type": "string"}, "to": {"type": "string"}}),
            &["node", "from", "to"],
        ),
        tool(
            "remove_path",
            "Delete a file on an SSH host, or a folder with everything in it. Needs files-write.",
            json!({"node": node, "path": {"type": "string"}}),
            &["node", "path"],
        ),
        tool(
            "start_tunnel",
            "Start a tunnel and wait until it is up. Needs tunnel.",
            json!({"node": node}),
            &["node"],
        ),
        tool(
            "monitor",
            "Show an SSH host's CPU, memory, disks, network, load and systemd services, with a summary of the last hour while the monitor has been open. For a host Burrow monitors in the background, also the alerts that hold now (failed services, full disks, low memory, not answering). Needs monitor-read, and burrow-agent installed on the host from Burrow.",
            json!({
                "node": node,
                "units": {"type": "string", "enum": ["failed", "all"], "description": "Which services to list, default failed"},
            }),
            &["node"],
        ),
        tool(
            "stop_tunnel",
            "Stop a running tunnel. Needs tunnel.",
            json!({"node": node}),
            &["node"],
        ),
    ])
}
