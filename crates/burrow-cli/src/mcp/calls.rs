//! The tools, each turned into the daemon command it stands for.

use burrow_core::{Command, Node, Response};
use serde_json::{Value, json};

use super::{Bridge, Called, Kind, pretty};

/// Rows a query keeps when the assistant doesn't say. Results go into its
/// context, so fewer than the window's default.
const DEFAULT_ROWS: u32 = 200;

impl Bridge {
    pub(super) async fn call(&mut self, tool: &str, args: &Value) -> Result<String, Called> {
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
                    Response::File { data, size, .. } => return Ok(file_text(&data, size)),
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
                    expect: None,
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
                let mut events = self.client.subscribe();
                let session = match self.execute(Command::StartTunnel { node: node.id }).await? {
                    Response::Session { session } => session,
                    other => return Ok(pretty(&other)),
                };
                self.wait(session, &mut events).await?;
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
