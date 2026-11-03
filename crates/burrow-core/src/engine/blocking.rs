//! The commands that run on a blocking thread: everything that reads or
//! writes the tree, the keyring, the state files (autostart, monitoring,
//! grants, history) or the addon host. One match, a command per arm.

use crate::api::{
    API_VERSION, Command, Event, NodeId, POSTGRES_KIND, Response, SSH_HOST_KIND, SSH_TUNNEL_KIND,
};
use crate::error::{Error, Result};
use crate::grants::{self};
use crate::secrets::SecretKey;
use crate::ssh::trend::{self};

use super::Inner;
use super::checks::{check_kind, validate_field};

impl Inner {
    pub(super) fn execute_inner(&self, command: Command) -> Result<Response> {
        match command {
            Command::Version => Ok(Response::Version { api: API_VERSION }),
            Command::ListNodes => Ok(Response::Nodes {
                nodes: self.tree().nodes(),
            }),
            Command::GetNode { id } => Ok(Response::Node {
                node: self.tree().get(id)?.node.clone(),
            }),
            Command::CreateNode {
                parent,
                kind,
                name,
                color,
                props,
            } => {
                let node = self.tree().create(parent, kind, name, color, props)?;
                self.emit(Event::NodeCreated { node: node.clone() });
                Ok(Response::Node { node })
            }
            Command::UpdateNode { id, patch } => {
                let mut tree = self.tree();
                let before = tree.get(id)?.node.clone();
                let node = tree.update(id, patch)?;
                if node != before {
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::MoveNode { id, parent } => {
                let mut tree = self.tree();
                let before = tree.get(id)?.node.parent;
                let node = tree.move_node(id, parent)?;
                if node.parent != before {
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::DeleteNode { id, recursive } => {
                let removed = self.tree().delete(id, recursive)?;
                for entry in &removed {
                    self.emit(Event::NodeDeleted { id: entry.node.id });
                }
                self.forget_secrets(&removed);
                let ids: Vec<NodeId> = removed.iter().map(|e| e.node.id).collect();
                self.autostart.forget(&ids);
                self.agents.forget(&ids);
                self.monitoring.forget(&ids);
                self.trends.forget(&ids);
                self.history.forget(&ids);
                Ok(Response::Ok)
            }
            Command::SetSecret { id, field, value } => {
                validate_field(&field)?;
                let key = match self.tree().get(id)?.secret_refs.get(&field) {
                    Some(reference) => SecretKey::for_node_ref(id, &field, reference)?,
                    None => SecretKey::for_field(id, &field),
                };
                // Store the value first, without holding the tree lock across
                // a D-Bus round trip, then record the reference.
                self.secrets.set(&key, &value)?;
                match self.tree().set_secret_ref(id, &field, Some(key.to_ref())) {
                    Ok(node) => {
                        self.emit(Event::NodeUpdated { node: node.clone() });
                        Ok(Response::Node { node })
                    }
                    Err(e) => {
                        // The node went away meanwhile; don't leave an orphan.
                        let _ = self.secrets.delete(&key);
                        Err(e)
                    }
                }
            }
            Command::ClearSecret { id, field } => {
                let (node, reference) = {
                    let mut tree = self.tree();
                    let reference = tree.get(id)?.secret_refs.get(&field).cloned();
                    (tree.set_secret_ref(id, &field, None)?, reference)
                };
                if let Some(reference) = reference {
                    // A reference that isn't this node's own is dropped from
                    // the file but never followed into the store.
                    if let Ok(key) = SecretKey::for_node_ref(id, &field, &reference) {
                        self.secrets.delete(&key)?;
                    }
                    self.emit(Event::NodeUpdated { node: node.clone() });
                }
                Ok(Response::Node { node })
            }
            Command::ListAddons => Ok(Response::Addons {
                addons: self.addons.infos(),
            }),
            Command::ApproveAddon { id, fingerprint } => {
                self.addons.approve(&id, &fingerprint)?;
                self.emit(Event::AddonsChanged {
                    addons: self.addons.infos(),
                });
                Ok(Response::Ok)
            }
            Command::RevokeAddon { id } => {
                self.addons.revoke(&id)?;
                self.emit(Event::AddonsChanged {
                    addons: self.addons.infos(),
                });
                Ok(Response::Ok)
            }
            Command::ListAutostart => Ok(Response::Autostart {
                nodes: self.autostart_nodes.get(),
            }),
            Command::SetAutostart {
                node,
                enabled,
                expect,
            } => {
                // The tunnel and its host are read under one lock, so the
                // consent covers settings that existed together. Holding
                // the lock while saving would invert the autostart lock's
                // order, so the consent is pinned to this snapshot instead:
                // a change after it withdraws the consent as usual.
                let snapshot = self.snapshot_with_via(node)?;
                let node = &snapshot[0];
                check_kind(node, SSH_TUNNEL_KIND)?;
                if enabled {
                    let expect = expect.ok_or_else(|| {
                        Error::invalid("expect", "enabling needs the settings the user was shown")
                    })?;
                    let host = snapshot.get(1);
                    let same = expect.props == node.props
                        && host.is_some_and(|host| {
                            expect.host_name == host.name && expect.host_props == host.props
                        });
                    if !same {
                        return Err(Error::invalid(
                            "tunnel",
                            "its settings changed since they were shown; look again",
                        ));
                    }
                }
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                self.autostart.set(&lookup, node, enabled)?;
                Ok(Response::Ok)
            }
            Command::MonitorTrend { node, range } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                Ok(Response::Trend {
                    points: self.trends.points(node.id, range, trend::now()),
                })
            }
            Command::ListAlerts => Ok(Response::Alerts {
                alerts: self.alerts.list(),
            }),
            Command::ListMonitoring => Ok(Response::Monitoring {
                nodes: self.monitoring_nodes.get(),
            }),
            Command::SetMonitoring {
                node,
                enabled,
                expect,
            } => {
                // Read under one lock, as for grants, so the choice covers
                // a host and a `via` that existed together.
                let snapshot = self.snapshot_with_via(node)?;
                let node = &snapshot[0];
                check_kind(node, SSH_HOST_KIND)?;
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                if enabled {
                    let expect = expect.ok_or_else(|| {
                        Error::invalid("expect", "turning it on needs the host the user was shown")
                    })?;
                    if !grants::matches(&lookup, node, &expect) {
                        return Err(Error::invalid(
                            "expect",
                            format!("{} changed after it was shown; show it again", node.name),
                        ));
                    }
                }
                self.monitoring.set(&lookup, node, enabled)?;
                Ok(Response::Ok)
            }
            Command::ListGrants => Ok(Response::Grants {
                grants: self.grants_shown.get(),
            }),
            Command::SetGrant {
                node,
                capabilities,
                expect,
            } => {
                // Read under one lock, as for autostart, so the grant
                // covers a node and a `via` that existed together.
                let snapshot = self.snapshot_with_via(node)?;
                let lookup = |id| snapshot.iter().find(|n| n.id == id).cloned();
                self.grants
                    .set(&lookup, &snapshot[0], capabilities, expect.as_ref())?;
                Ok(Response::Ok)
            }
            Command::ListProblems => Ok(Response::Problems {
                problems: self.problems.get(),
            }),
            Command::ReloadConfig => {
                self.reload()?;
                Ok(Response::Ok)
            }
            Command::ListHistory { node } => {
                self.node_of_kind(node, POSTGRES_KIND)?;
                Ok(Response::History {
                    entries: self.history.list(node),
                })
            }
            Command::ClearHistory { node } => {
                self.node_of_kind(node, POSTGRES_KIND)?;
                self.history.forget(&[node]);
                Ok(Response::Ok)
            }
            // Handled by `Core::execute` before it gets here.
            Command::OpenTerminal { .. }
            | Command::WriteTerminal { .. }
            | Command::ResizeTerminal { .. }
            | Command::AnswerPrompt { .. }
            | Command::CloseSession { .. }
            | Command::ListSessions
            | Command::StartTunnel { .. }
            | Command::OpenFiles { .. }
            | Command::Exec { .. }
            | Command::ControlService { .. }
            | Command::ListDir { .. }
            | Command::ReadFile { .. }
            | Command::WriteFile { .. }
            | Command::MakeDir { .. }
            | Command::RenamePath { .. }
            | Command::RemovePath { .. }
            | Command::Download { .. }
            | Command::Upload { .. }
            | Command::CancelTransfer { .. }
            | Command::OpenDatabase { .. }
            | Command::OpenVnc { .. }
            | Command::RunQuery { .. }
            | Command::CancelQuery { .. }
            | Command::SetReadOnly { .. }
            | Command::ListSchema { .. }
            | Command::ExportQuery { .. }
            | Command::OpenView { .. }
            | Command::OpenMonitor { .. }
            | Command::MonitorHistory { .. }
            | Command::InvokeAction { .. }
            | Command::CheckSshInclude { .. }
            | Command::AddSshHost { .. } => unreachable!("async command on the blocking path"),
        }
    }
}
