//! modular-exempt: one exhaustive match routes every `Command`, and a
//! match is not split by line range.
//!
//! Running one command: the session commands are handed to their running
//! session, SSH hosts are added here (the OpenSSH config and the node, as
//! one step), and the rest goes to a blocking thread (`blocking`).

use crate::api::{
    Command, NodeId, POSTGRES_KIND, Response, SSH_HOST_KIND, SSH_TUNNEL_KIND, SessionKind,
    TerminalRun, VNC_KIND,
};
use crate::db;
use crate::error::{Error, Result};
use crate::ssh::{self, Input, files, hosts, services};
use crate::vnc;

use super::{CONTROL_TIMEOUT, Core};

impl Core {
    /// Runs one command. File and keyring I/O happen on a blocking thread so
    /// callers on an async runtime are never stalled. Session commands are
    /// only messages to a running session and are handled directly.
    pub async fn execute(&self, command: Command) -> Result<Response> {
        let sessions = &self.inner.sessions;
        let (session, input) = match command {
            Command::OpenTerminal {
                node,
                cols,
                rows,
                run,
            } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let command = match &run {
                    None => None,
                    Some(TerminalRun::Logs { service }) => Some(services::logs(service)),
                    Some(TerminalRun::Control { service, action }) => {
                        Some(services::control(service, *action, true))
                    }
                };
                let command = command.transpose().map_err(|e| Error::invalid("run", e))?;
                let session = ssh::open_terminal(self, node, cols, rows, command);
                return Ok(Response::Session { session });
            }
            Command::ControlService {
                node,
                service,
                action,
            } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let command = services::control(&service, action, false)
                    .map_err(|e| Error::invalid("service", e))?;
                let result = ssh::exec::start(self, node, command, Some(CONTROL_TIMEOUT))?
                    .await
                    .map_err(|_| Error::Remote("the command was stopped".to_string()))?
                    .map_err(Error::Remote)?;
                let needs_password = services::needs_password(result.status, &result.stderr);
                return Ok(Response::Controlled {
                    result,
                    needs_password,
                });
            }
            Command::StartTunnel { node } => {
                let node = self.node_of_kind(node, SSH_TUNNEL_KIND)?;
                let session = match sessions.find(node.id, SessionKind::Tunnel) {
                    Some(session) => session,
                    None => ssh::open_tunnel(self, node),
                };
                return Ok(Response::Session { session });
            }
            Command::Exec {
                node,
                command,
                timeout,
            } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let result = ssh::exec::start(self, node, command, timeout)?
                    .await
                    .map_err(|_| Error::Remote("the command was stopped".to_string()))?
                    .map_err(Error::Remote)?;
                return Ok(Response::Exec { result });
            }
            Command::OpenFiles { node } => {
                let node = self.node_of_kind(node, SSH_HOST_KIND)?;
                let session = ssh::open_files(self, node);
                return Ok(Response::Session { session });
            }
            Command::OpenMonitor { node } => {
                let session = self.open_monitor(node, true)?;
                return Ok(Response::Session { session });
            }
            Command::SetMonitoring {
                node,
                enabled,
                expect,
            } => {
                let command = Command::SetMonitoring {
                    node,
                    enabled,
                    expect,
                };
                let response = self.blocking(command).await?;
                if enabled {
                    self.start_monitoring(node);
                }
                return Ok(response);
            }
            Command::MonitorHistory { session } => {
                let history = sessions.monitor(session)?.history();
                return Ok(Response::Monitor { history });
            }
            Command::ListDir { session, path } => {
                let sftp = sessions.sftp(session)?;
                let (path, entries) = files::list_dir(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Dir { path, entries });
            }
            Command::ReadFile {
                session,
                path,
                limit,
            } => {
                let sftp = sessions.sftp(session)?;
                let limit = limit.unwrap_or(1 << 20);
                let (data, size) = files::read_file(&sftp, &path, limit)
                    .await
                    .map_err(Error::Remote)?;
                let sha256 = crate::api::sha256_hex(&data);
                return Ok(Response::File { data, size, sha256 });
            }
            Command::WriteFile {
                session,
                path,
                data,
                overwrite,
                expect,
            } => {
                let sftp = sessions.sftp(session)?;
                files::write_file(&sftp, &path, &data, overwrite, expect.as_deref())
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::MakeDir { session, path } => {
                let sftp = sessions.sftp(session)?;
                files::make_dir(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::RenamePath { session, from, to } => {
                let sftp = sessions.sftp(session)?;
                files::rename(&sftp, &from, &to)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::RemovePath { session, path } => {
                let sftp = sessions.sftp(session)?;
                files::remove(&sftp, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::Download {
                session,
                remote,
                local,
                overwrite,
            } => {
                let sftp = sessions.sftp(session)?;
                let direction = files::Direction::Download { remote, local };
                let transfer = files::start(self, session, sftp, direction, overwrite);
                return Ok(Response::Transfer { transfer });
            }
            Command::Upload {
                session,
                local,
                remote,
                overwrite,
            } => {
                let sftp = sessions.sftp(session)?;
                let direction = files::Direction::Upload { local, remote };
                let transfer = files::start(self, session, sftp, direction, overwrite);
                return Ok(Response::Transfer { transfer });
            }
            Command::CancelTransfer { transfer } => {
                self.inner.transfers.cancel(transfer)?;
                return Ok(Response::Ok);
            }
            Command::OpenDatabase { node } => {
                let node = self.node_of_kind(node, POSTGRES_KIND)?;
                let session = db::open(self, node);
                return Ok(Response::Session { session });
            }
            Command::OpenVnc { node } => {
                let node = self.node_of_kind(node, VNC_KIND)?;
                let session = vnc::open(self, node);
                return Ok(Response::Session { session });
            }
            Command::RunQuery {
                session,
                sql,
                limit,
            } => return self.run_query(session, sql, limit, false).await,
            Command::CancelQuery { session } => {
                let (handle, _) = sessions.database(session)?;
                handle.cancel().await.map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::SetReadOnly { session, read_only } => {
                let (handle, _) = sessions.database(session)?;
                handle
                    .set_read_only(read_only)
                    .await
                    .map_err(Error::Remote)?;
                return Ok(Response::Ok);
            }
            Command::ListSchema { session } => {
                let (handle, _) = sessions.database(session)?;
                let schemas = handle.schema().await.map_err(Error::Remote)?;
                return Ok(Response::Schema { schemas });
            }
            Command::ExportQuery { session, sql, path } => {
                let (handle, _) = sessions.database(session)?;
                let rows = handle.export(&sql, &path).await.map_err(Error::Remote)?;
                return Ok(Response::Exported { rows });
            }
            Command::ListSessions => {
                return Ok(Response::Sessions {
                    sessions: sessions.list(),
                });
            }
            Command::AnswerPrompt {
                session,
                prompt,
                answer,
            } => {
                sessions.answer(session, prompt, answer)?;
                return Ok(Response::Ok);
            }
            Command::WriteTerminal { session, data } => (session, Input::Write(data)),
            Command::ResizeTerminal {
                session,
                cols,
                rows,
            } => (session, Input::Resize { cols, rows }),
            Command::CloseSession { session } => (session, Input::Close),
            Command::OpenView { node } => {
                let node = self.inner.tree().get(node)?.node.clone();
                let manifest = self.inner.addons.owner(&node.kind).ok_or_else(|| {
                    Error::invalid("node", format!("no addon handles {} nodes", node.kind))
                })?;
                let view = self.inner.addons.open_view(self, &manifest, node).await?;
                return Ok(Response::View { view });
            }
            Command::InvokeAction {
                addon,
                action,
                node,
                values,
            } => {
                let manifest = self.inner.addons.manifest(&addon)?;
                let node = match node {
                    Some(id) => Some(self.inner.tree().get(id)?.node.clone()),
                    None => None,
                };
                let offered = match &node {
                    // Buttons and forms in its own views can send anything.
                    Some(node) if manifest.owns(&node.kind) => true,
                    Some(node) => manifest
                        .actions
                        .iter()
                        .any(|a| a.id == action && a.kinds.contains(&node.kind)),
                    None => manifest.commands.iter().any(|c| c.id == action),
                };
                if !offered {
                    return Err(Error::invalid(
                        "action",
                        format!("addon {addon} offers no {action:?} here"),
                    ));
                }
                let message = self
                    .inner
                    .addons
                    .invoke(self, &manifest, &action, node, values)
                    .await?;
                return Ok(Response::Invoked { message });
            }
            Command::CheckSshInclude { alias } => {
                let status = hosts::include_status(self.ssh_config(), &alias).await?;
                return Ok(Response::SshInclude {
                    included: status.included,
                    config: status.config,
                    line: status.line,
                });
            }
            Command::AddSshHost {
                parent,
                alias,
                hostname,
                user,
                port,
                identity_file,
                add_include,
            } => {
                let host = hosts::NewHost {
                    alias,
                    hostname,
                    user,
                    port,
                    identity_file,
                };
                return self.add_ssh_host(parent, host, add_include).await;
            }
            command => return self.blocking(command).await,
        };
        sessions.send(session, input)?;
        Ok(Response::Ok)
    }

    async fn blocking(&self, command: Command) -> Result<Response> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.execute(command))
            .await
            .map_err(|e| Error::Task(e.to_string()))?
    }

    /// Writes the host to the OpenSSH config, then makes its node. The
    /// block is taken back if the node can't be made, so a failure leaves
    /// both as they were.
    async fn add_ssh_host(
        &self,
        parent: Option<NodeId>,
        host: hosts::NewHost,
        add_include: bool,
    ) -> Result<Response> {
        let _adding = self.inner.adding_host.lock().await;
        // Checked first too, so the common mistake writes nothing.
        if let Some(parent) = parent
            && !self.node(parent)?.is_folder()
        {
            return Err(Error::NotAFolder(parent));
        }
        let added = hosts::add(self.ssh_config(), &host, add_include).await?;
        let mut props = toml::Table::new();
        props.insert("alias".into(), host.alias.clone().into());
        let command = Command::CreateNode {
            parent,
            kind: SSH_HOST_KIND.into(),
            name: host.alias,
            color: None,
            props,
        };
        let inner = self.inner.clone();
        let result = tokio::task::spawn_blocking(move || inner.execute(command))
            .await
            .map_err(|e| Error::Task(e.to_string()))
            .flatten();
        if result.is_err() {
            added.undo();
        }
        result
    }
}
