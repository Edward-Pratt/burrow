//! SSH connections, shared by everything open on a host: terminals,
//! tunnels and file panels each open their own channels on one connection,
//! so the user signs in once. A connection closes when the last of them
//! lets go.

mod auth;

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use russh::client::{self, DisconnectReason, Handle};
use russh::keys::{HashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{Disconnect, Preferred};
use tokio::net::TcpStream;
use tokio::sync::watch;

use super::Prompter;
use super::config::{self, HostConfig, StrictHostKeyChecking};
use super::known_hosts::{self, Check};
use crate::api::{Node, NodeId, PromptAnswer};
use crate::engine::Core;

/// Used when the OpenSSH config sets no `ConnectTimeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

type Result<T> = std::result::Result<T, String>;

fn text(error: impl Display) -> String {
    error.to_string()
}

/// A signed-in connection. Cheap to share; it disconnects when the last
/// `Arc` is dropped.
pub(crate) struct Connection {
    handle: Arc<Handle<Client>>,
    /// Set to why the connection ended, once it has.
    ended: watch::Receiver<Option<String>>,
}

impl Connection {
    pub fn handle(&self) -> &Handle<Client> {
        &self.handle
    }

    /// Waits for the connection to end, and says why.
    pub async fn closed(&self) -> String {
        let mut ended = self.ended.clone();
        match ended.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => "the connection closed".into(),
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        let handle = self.handle.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
            });
        }
    }
}

/// The open connection of each host, by node and alias: editing a node's
/// alias makes later sessions connect afresh.
#[derive(Default)]
pub(crate) struct Connections {
    map: Mutex<HashMap<(NodeId, String), Arc<Slot>>>,
}

#[derive(Default)]
struct Slot {
    /// Held while connecting, so a second session waits for the first one's
    /// connection instead of signing in again.
    current: tokio::sync::Mutex<Weak<Connection>>,
}

impl Connections {
    /// The host's open connection, or a new one. Questions asked while
    /// connecting (host key, passwords) go to `prompter`'s session. If that
    /// session closes part way, whoever else was waiting connects in turn
    /// and is asked instead.
    pub async fn get(
        &self,
        core: &Core,
        node: &Node,
        prompter: &Prompter,
        step: &impl Fn(String),
    ) -> Result<Arc<Connection>> {
        let alias = alias(node).to_string();
        let slot = {
            let mut map = self.map.lock().unwrap();
            // Forget hosts nobody is connected to or waiting for.
            map.retain(|_, slot| {
                Arc::strong_count(slot) > 1
                    || slot
                        .current
                        .try_lock()
                        .map_or(true, |current| current.strong_count() > 0)
            });
            map.entry((node.id, alias.clone())).or_default().clone()
        };
        let mut current = slot.current.lock().await;
        if let Some(connection) = current.upgrade()
            && !connection.handle.is_closed()
        {
            step(format!("Using the open connection to {alias}"));
            return Ok(connection);
        }
        let connection = Arc::new(connect(core, node, prompter, step).await?);
        *current = Arc::downgrade(&connection);
        Ok(connection)
    }
}

/// `props.alias`, or the node's name when it has none.
pub(crate) fn alias(node: &Node) -> &str {
    node.props
        .get("alias")
        .and_then(|v| v.as_str())
        .unwrap_or(&node.name)
}

async fn connect(
    core: &Core,
    node: &Node,
    prompter: &Prompter,
    step: &impl Fn(String),
) -> Result<Connection> {
    let alias = alias(node);
    step(format!("Reading the OpenSSH config for {alias}"));
    let config = Arc::new(config::resolve(alias, core.ssh_config()).await?);
    // Connecting straight to the host name would reach the wrong network
    // or hang, so refuse rather than ignore these.
    if let Some(jump) = &config.proxy_jump {
        return Err(format!(
            "{alias} connects through ProxyJump {jump}, which Burrow doesn't support yet"
        ));
    }
    if config.proxy_command.is_some() {
        return Err(format!(
            "{alias} uses a ProxyCommand, which Burrow doesn't support yet"
        ));
    }

    let address = format!("{}:{}", config.hostname, config.port);
    step(format!("Connecting to {address}"));
    let timeout = config.connect_timeout.unwrap_or(CONNECT_TIMEOUT);
    let socket = tokio::time::timeout(
        timeout,
        TcpStream::connect((config.hostname.as_str(), config.port)),
    )
    .await
    .map_err(|_| format!("{address} did not answer within {} s", timeout.as_secs()))?
    .map_err(|e| format!("could not connect to {address}: {e}"))?;
    let _ = socket.set_nodelay(true);

    let rejection = Arc::new(Mutex::new(None));
    let (ended_tx, ended) = watch::channel(None);
    let client = Client {
        config: config.clone(),
        prompter: prompter.clone(),
        rejection: rejection.clone(),
        ended: ended_tx,
    };
    // Ask for a key type known_hosts already has, so a known host isn't
    // mistaken for a new one because it offered a different type first.
    let known = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || {
            let files: Vec<_> = config
                .user_known_hosts
                .iter()
                .chain(&config.global_known_hosts)
                .cloned()
                .collect();
            known_hosts::known_algorithms(&files, config.host_key_name(), config.port)
        })
        .await
        .map_err(text)?
    };
    let mut preferred = Preferred::default();
    let mut keys = preferred.key.to_vec();
    keys.sort_by_key(|algorithm| !known.contains(algorithm));
    preferred.key = keys.into();
    let russh_config = client::Config {
        keepalive_interval: config.server_alive_interval,
        preferred,
        ..Default::default()
    };
    step("Checking the host key".into());
    let mut handle = match client::connect_stream(Arc::new(russh_config), socket, client).await {
        Ok(handle) => handle,
        // A refused host key surfaces as a generic error; say why instead.
        Err(e) => return Err(rejection.lock().unwrap().take().unwrap_or_else(|| text(e))),
    };

    step(format!("Signing in as {}", config.user));
    auth::authenticate(&mut handle, &config, core, node, prompter).await?;
    Ok(Connection {
        handle: Arc::new(handle),
        ended,
    })
}

pub(crate) struct Client {
    config: Arc<HostConfig>,
    /// Asks the session that connected. Only used while connecting.
    prompter: Prompter,
    /// Why the host key was refused, for the error message.
    rejection: Arc<Mutex<Option<String>>>,
    ended: watch::Sender<Option<String>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // Covers every way the connection ends, including a failed
        // handshake, which `disconnected` is not told about.
        self.ended.send_if_modified(|reason| {
            if reason.is_some() {
                return false;
            }
            *reason = Some("the connection closed".into());
            true
        });
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> std::result::Result<bool, Self::Error> {
        let result = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => {
                verify_host_key(&self.config, key, &self.prompter).await
            }
            PublicKeyOrCertificate::Certificate(_) => {
                Err("the server offered a host certificate, which Burrow doesn't check yet".into())
            }
        };
        match result {
            Ok(()) => Ok(true),
            Err(reason) => {
                *self.rejection.lock().unwrap() = Some(reason);
                Ok(false)
            }
        }
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> std::result::Result<(), Self::Error> {
        let message = match &reason {
            DisconnectReason::ReceivedDisconnect(info) if info.message.is_empty() => {
                "the server closed the connection".to_string()
            }
            DisconnectReason::ReceivedDisconnect(info) => {
                format!("the server closed the connection: {}", info.message)
            }
            DisconnectReason::Error(e) => format!("the connection failed: {e}"),
        };
        self.ended.send_replace(Some(message));
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

/// Checks the key against `known_hosts` the way OpenSSH does, asking the
/// user about a new key when `StrictHostKeyChecking` is `ask`.
async fn verify_host_key(config: &HostConfig, key: &PublicKey, prompter: &Prompter) -> Result<()> {
    let name = config.host_key_name().to_string();
    let port = config.port;
    let files: Vec<_> = config
        .user_known_hosts
        .iter()
        .chain(&config.global_known_hosts)
        .cloned()
        .collect();
    let check = {
        let (name, key) = (name.clone(), key.clone());
        tokio::task::spawn_blocking(move || known_hosts::check(&files, &name, port, &key))
            .await
            .map_err(text)?
    };
    let fingerprint = key.fingerprint(HashAlg::Sha256);
    match check.clone() {
        Check::Known => return Ok(()),
        Check::Revoked { path, line } => {
            return Err(format!(
                "the host key for {name} is marked as revoked ({}, line {line})",
                path.display()
            ));
        }
        Check::Changed { path, line } => {
            return Err(format!(
                "THE HOST KEY FOR {name} HAS CHANGED. Someone may be intercepting the \
                 connection, or the server was reinstalled. Burrow will not connect. The \
                 server now offers {} key {fingerprint}; the old key is in {}, line {line}. \
                 If you know why it changed, remove the old key with `ssh-keygen -R {}` \
                 and connect again.",
                key.algorithm(),
                path.display(),
                if port == 22 {
                    name.clone()
                } else {
                    format!("'[{name}]:{port}'")
                },
            ));
        }
        Check::Unknown | Check::OtherTypes { .. } => {}
    }
    // Checked before asking, so the user isn't asked to trust a key Burrow
    // then can't record.
    known_hosts::check_host_name(&name)?;

    // Only a host with no known keys at all is quietly accepted under
    // `accept-new`. One that offers an unfamiliar key type always asks.
    let warning = match &check {
        Check::OtherTypes { known } => Some(format!(
            "Burrow knows a {} key for {name}, but the server offered a {} key \
             instead. That can happen after a server change, or when someone is \
             intercepting the connection.\n\n",
            known.join(" / "),
            key.algorithm()
        )),
        _ => None,
    };
    match (config.strict_host_key_checking, &warning) {
        (StrictHostKeyChecking::Yes, _) => {
            return Err(format!(
                "the {} key for {name} is not in known_hosts and StrictHostKeyChecking is on",
                key.algorithm()
            ));
        }
        (StrictHostKeyChecking::AcceptNew, None) => {}
        (StrictHostKeyChecking::AcceptNew | StrictHostKeyChecking::Ask, warning) => {
            let message = format!(
                "{}{}\n\n\
                 {} key fingerprint:\n{fingerprint}\n\n\
                 Only trust it if this matches the server's real key.",
                warning.as_deref().unwrap_or_default(),
                if warning.is_some() {
                    "Check this key with the server's administrator."
                } else {
                    "Burrow has not connected to this host before."
                },
                key.algorithm()
            );
            let answer = prompter
                .confirm("Trust this host?", &message, "Trust and connect")
                .await;
            if answer != PromptAnswer::Confirm {
                return Err(format!("the host key for {name} was not trusted"));
            }
        }
    }

    if let Some(path) = config.user_known_hosts.first() {
        let (path, key, hashed) = (path.clone(), key.clone(), config.hash_known_hosts);
        let learned = tokio::task::spawn_blocking(move || {
            known_hosts::learn(&path, &name, port, &key, hashed).map_err(|e| (path, e))
        })
        .await
        .map_err(text)?;
        // Like OpenSSH: warn, but the key was accepted for this connection.
        if let Err((path, e)) = learned {
            tracing::warn!("could not add the host key to {}: {e}", path.display());
        }
    }
    Ok(())
}
