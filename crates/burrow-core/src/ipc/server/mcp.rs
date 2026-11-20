//! What the MCP bridge may run, checked against its grants, and the audit
//! log of what it sent.

use serde_json::json;

use super::Conn;
use crate::api::{Capability, Command, NodeId, Response, SessionId};
use crate::class::McpRule;
use crate::error::{Error, Result};
use crate::ipc::audit::{Rule, audit_entry, cap_strings, command_type, cut_error};

impl Conn {
    /// Runs a command for the MCP bridge if its grants allow it, and
    /// records it in the audit log either way.
    pub(super) async fn execute_mcp(&self, command: Command) -> Result<Response> {
        let rule = self.mcp_rule(&command);
        let mut entry = audit_entry(&command, rule.as_ref().ok(), self.id);
        let result = match rule {
            Err(e) => Err(e),
            Ok(Rule::Run) => self.core.execute(command).await,
            Ok(Rule::ReadOnlyQuery) => match command {
                Command::RunQuery {
                    session,
                    sql,
                    limit,
                } => self.core.run_query(session, sql, limit, true).await,
                _ => unreachable!("only queries are read-only"),
            },
            Ok(Rule::MonitorWithoutInstall) => match command {
                Command::OpenMonitor { node } => self
                    .core
                    .open_monitor(node, false)
                    .map(|session| Response::Session { session }),
                _ => unreachable!("only monitors install"),
            },
            Ok(Rule::GrantedAlerts) => {
                self.core
                    .execute(command)
                    .await
                    .map(|response| match response {
                        Response::Alerts { alerts } => Response::Alerts {
                            alerts: alerts
                                .into_iter()
                                .filter(|alert| {
                                    self.core.granted(alert.node, Capability::MonitorRead)
                                })
                                .collect(),
                        },
                        other => other,
                    })
            }
        };
        entry["ok"] = json!(result.is_ok());
        match &result {
            // Commands on the session name only its id, so this ties them
            // to the node it was opened on.
            Ok(Response::Session { session }) => entry["session"] = json!(session),
            Ok(_) => {}
            Err(e) => entry["error"] = json!(cut_error(&e.to_string())),
        }
        cap_strings(&mut entry["command"]);
        self.core.audit(&entry);
        result
    }

    /// What the MCP bridge may run. The command's class says what it
    /// needs (see [`McpRule`]); this checks that against the grants and
    /// the sessions the bridge opened itself.
    fn mcp_rule(&self, command: &Command) -> Result<Rule> {
        match crate::class::of(command).mcp {
            McpRule::Free => {}
            McpRule::Node(node, capability) => self.require(node, capability)?,
            McpRule::Session(session, capability) => {
                let node = self.own_session(session)?;
                self.require(node, capability)?;
            }
            McpRule::Query(session) => {
                let node = self.own_session(session)?;
                self.require(node, Capability::SqlRead)?;
                if !self.core.granted(node, Capability::SqlWrite) {
                    return Ok(Rule::ReadOnlyQuery);
                }
            }
            McpRule::Monitor(node) => {
                self.require(node, Capability::MonitorRead)?;
                return Ok(Rule::MonitorWithoutInstall);
            }
            McpRule::Alerts => return Ok(Rule::GrantedAlerts),
            McpRule::Close(session) => {
                if self.own_session(session).is_err() {
                    let tunnel = self.core.sessions().list().into_iter().find(|i| {
                        i.session == session && i.kind == crate::api::SessionKind::Tunnel
                    });
                    match tunnel {
                        Some(info) => self.require(info.node, Capability::Tunnel)?,
                        None => return Err(denied("close a session it did not open")),
                    }
                }
            }
            McpRule::Denied => return Err(denied(&format!("run {}", command_type(command)))),
        }
        Ok(Rule::Run)
    }

    fn require(&self, node: NodeId, capability: Capability) -> Result<()> {
        if self.core.granted(node, capability) {
            return Ok(());
        }
        Err(denied(&format!(
            "use {capability} on node {node}; the user can grant it in Burrow"
        )))
    }

    /// The node of a running session this client opened.
    fn own_session(&self, session: SessionId) -> Result<NodeId> {
        if !self.owned.lock().unwrap().contains(&session) {
            return Err(denied("use a session it did not open"));
        }
        self.core
            .sessions()
            .list()
            .into_iter()
            .find(|info| info.session == session)
            .map(|info| info.node)
            .ok_or(Error::SessionNotFound(session))
    }
}

fn denied(what: &str) -> Error {
    Error::PermissionDenied(format!("the MCP bridge may not {what}"))
}
