//! One SSH terminal session: connect, check the host key, sign in, then
//! pass bytes between the remote shell and the client.

use std::fmt::Display;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use russh::client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect, MethodKind, MethodSet, Preferred};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

use super::config::{self, HostConfig, StrictHostKeyChecking};
use super::known_hosts::{self, Check};
use super::{Input, Prompter};
use crate::api::{Command, Event, Node, PromptAnswer, PromptKind, SecretValue, SessionState};
use crate::engine::Core;

/// Used when the OpenSSH config sets no `ConnectTimeout`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Wrong passwords or passphrases allowed before giving up, as OpenSSH.
const ATTEMPTS: usize = 3;

type Result<T> = std::result::Result<T, String>;

fn text(error: impl Display) -> String {
    error.to_string()
}

/// Runs the session to the end. `Err` carries a message for the user.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    (mut cols, mut rows): (u16, u16),
    mut input: mpsc::UnboundedReceiver<Input>,
    output: mpsc::Sender<Vec<u8>>,
    prompter: &Prompter,
) -> Result<()> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };

    // Connecting can wait on the network or on the user, so watch for a
    // close meanwhile. Keystrokes typed before the shell starts are dropped;
    // resizes are kept.
    let setup = connect(core, node, prompter, &step);
    let closed = async {
        loop {
            match input.recv().await {
                Some(Input::Resize { cols: c, rows: r }) => (cols, rows) = (c, r),
                Some(Input::Write(_)) => {}
                Some(Input::Close) | None => return,
            }
        }
    };
    let handle = tokio::select! {
        handle = setup => handle?,
        () = closed => return Ok(()),
    };

    let mut channel = handle.channel_open_session().await.map_err(text)?;
    channel
        .request_pty(false, "xterm-256color", cols.into(), rows.into(), 0, 0, &[])
        .await
        .map_err(text)?;
    channel.request_shell(false).await.map_err(text)?;
    core.emit(Event::SessionChanged {
        session: prompter.session,
        node: node.id,
        state: SessionState::Connected,
    });

    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    // The client stopped reading: nobody is watching.
                    if output.send(data.to_vec()).await.is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::Close) | None => break,
                Some(_) => {}
            },
            message = input.recv() => match message {
                Some(Input::Write(data)) => channel.data(&data[..]).await.map_err(text)?,
                Some(Input::Resize { cols, rows }) => channel
                    .window_change(cols.into(), rows.into(), 0, 0)
                    .await
                    .map_err(text)?,
                Some(Input::Close) | None => break,
            },
        }
    }
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

async fn connect(
    core: &Core,
    node: &Node,
    prompter: &Prompter,
    step: &impl Fn(String),
) -> Result<Handle<Client>> {
    let alias = node
        .props
        .get("alias")
        .and_then(|v| v.as_str())
        .unwrap_or(&node.name);
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
    let client = Client {
        config: config.clone(),
        prompter: prompter.clone(),
        rejection: rejection.clone(),
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
    authenticate(&mut handle, &config, core, node, prompter).await?;
    Ok(handle)
}

struct Client {
    config: Arc<HostConfig>,
    prompter: Prompter,
    /// Why the host key was refused, for the error message.
    rejection: Arc<Mutex<Option<String>>>,
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
                .ask(PromptKind::Confirm, "Trust this host?", &message)
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

/// Signs in the way OpenSSH would try: agent keys, then key files, then a
/// password, then keyboard-interactive.
async fn authenticate(
    handle: &mut Handle<Client>,
    config: &HostConfig,
    core: &Core,
    node: &Node,
    prompter: &Prompter,
) -> Result<()> {
    let user = config.user.as_str();
    let host = config.hostname.as_str();
    // The server's list of methods still worth trying, once it has said.
    let mut methods: Option<MethodSet> = None;
    let allowed = |methods: &Option<MethodSet>, kind: MethodKind| {
        methods.as_ref().is_none_or(|m| m.contains(&kind))
    };
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    let hash_for = |key: &PublicKey| key.algorithm().is_rsa().then_some(rsa_hash).flatten();

    // The public halves of the configured key files.
    let configured: Vec<(&Path, Option<PublicKey>)> = config
        .identity_files
        .iter()
        .map(|path| {
            // `IdentityFile` may name the public key itself, to pick a key
            // held only by the agent (1Password's setup does this).
            let public = russh::keys::load_public_key(path)
                .or_else(|_| russh::keys::load_public_key(path.with_added_extension("pub")))
                .ok();
            (path.as_path(), public)
        })
        .collect();
    let is_configured = |key: &PublicKey| {
        configured
            .iter()
            .any(|(_, p)| p.as_ref().is_some_and(|p| same(p, key)))
    };
    let mut tried: Vec<PublicKey> = Vec::new();

    // 1. Keys in the agent. Agents can hold many keys, and each one offered
    // counts against the server's MaxAuthTries, so configured keys go first.
    if let Some(socket) = &config.identity_agent {
        match AgentClient::connect_uds(socket).await {
            Ok(mut agent) => {
                let mut keys: Vec<PublicKey> = agent
                    .request_identities()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|identity| match identity {
                        AgentIdentity::PublicKey { key, .. } => Some(key),
                        AgentIdentity::Certificate { .. } => None,
                    })
                    .collect();
                if config.identities_only {
                    keys.retain(is_configured);
                }
                keys.sort_by_key(|key| !is_configured(key));
                for key in keys {
                    if !allowed(&methods, MethodKind::PublicKey) {
                        break;
                    }
                    let hash = hash_for(&key);
                    match handle
                        .authenticate_publickey_with(user, key.clone(), hash, &mut agent)
                        .await
                    {
                        Ok(AuthResult::Success) => return Ok(()),
                        Ok(AuthResult::Failure {
                            remaining_methods, ..
                        }) => methods = Some(remaining_methods),
                        // The agent declined to sign, for example when its
                        // approval dialog was dismissed.
                        Err(e) => tracing::debug!("agent key not used: {e}"),
                    }
                    tried.push(key);
                }
            }
            Err(e) => tracing::debug!("no SSH agent at {}: {e}", socket.display()),
        }
    }

    // 2. Key files.
    for (path, public) in &configured {
        if !allowed(&methods, MethodKind::PublicKey) {
            break;
        }
        if public
            .as_ref()
            .is_some_and(|p| tried.iter().any(|t| same(t, p)))
        {
            continue;
        }
        let Some(key) = load_key(path, prompter).await? else {
            continue;
        };
        if tried.iter().any(|t| same(t, key.public_key())) {
            continue;
        }
        let hash = hash_for(key.public_key());
        tried.push(key.public_key().clone());
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        match handle
            .authenticate_publickey(user, key)
            .await
            .map_err(text)?
        {
            AuthResult::Success => return Ok(()),
            AuthResult::Failure {
                remaining_methods, ..
            } => methods = Some(remaining_methods),
        }
    }

    // 3. A password: the saved one first, then ask.
    if allowed(&methods, MethodKind::Password) {
        let target = password_target(config);
        let saved = saved_password(core, node, &target).await;
        for attempt in 0..ATTEMPTS + usize::from(matches!(saved, Saved::Matches(_))) {
            let (password, save) = match (&saved, attempt) {
                (Saved::Matches(saved), 0) => (saved.clone(), false),
                _ => {
                    let message = match (&saved, attempt) {
                        (Saved::ForElsewhere(other), 0) => format!(
                            "The saved password is for {other}, so Burrow didn't send it. \
                             Password for {target}"
                        ),
                        (_, 0) => format!("Password for {target}"),
                        _ => format!("That didn't work. Password for {target}"),
                    };
                    match prompter
                        .ask(PromptKind::Secret { can_save: true }, "Password", &message)
                        .await
                    {
                        PromptAnswer::Text { value, save } => (value, save),
                        _ => return Err("signing in was cancelled".into()),
                    }
                }
            };
            match handle
                .authenticate_password(user, password.expose())
                .await
                .map_err(text)?
            {
                AuthResult::Success => {
                    if save {
                        save_password(core, node, password, target).await;
                    }
                    return Ok(());
                }
                AuthResult::Failure {
                    remaining_methods, ..
                } => {
                    let still = remaining_methods.contains(&MethodKind::Password);
                    methods = Some(remaining_methods);
                    if !still {
                        break;
                    }
                }
            }
        }
    }

    // 4. Keyboard-interactive: whatever the server asks (often a password,
    // sometimes a one-time code).
    if allowed(&methods, MethodKind::KeyboardInteractive) {
        let mut response = handle
            .authenticate_keyboard_interactive_start(user, None)
            .await
            .map_err(text)?;
        loop {
            let (name, instructions, prompts) = match response {
                KeyboardInteractiveAuthResponse::Success => return Ok(()),
                KeyboardInteractiveAuthResponse::Failure { .. } => break,
                KeyboardInteractiveAuthResponse::InfoRequest {
                    name,
                    instructions,
                    prompts,
                } => (name, instructions, prompts),
            };
            let title = if name.is_empty() {
                format!("Sign in to {host}")
            } else {
                name
            };
            let mut answers = Vec::new();
            for prompt in prompts {
                let kind = if prompt.echo {
                    PromptKind::Text
                } else {
                    PromptKind::Secret { can_save: false }
                };
                let message = [instructions.trim(), prompt.prompt.trim()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join("\n\n");
                match prompter.ask(kind, &title, &message).await {
                    PromptAnswer::Text { value, .. } => answers.push(value.expose().to_string()),
                    _ => return Err("signing in was cancelled".into()),
                }
            }
            response = handle
                .authenticate_keyboard_interactive_respond(answers)
                .await
                .map_err(text)?;
        }
    }

    Err(format!(
        "{user}@{host} did not accept any of the keys or passwords Burrow tried"
    ))
}

fn same(a: &PublicKey, b: &PublicKey) -> bool {
    a.key_data() == b.key_data()
}

/// Loads a private key file, asking for its passphrase if it has one.
/// `None` skips the file: it is missing, of a type russh can't use, or the
/// user cancelled.
async fn load_key(path: &Path, prompter: &Prompter) -> Result<Option<PrivateKey>> {
    if !path.exists() {
        return Ok(None);
    }
    let read = |passphrase: Option<SecretValue>| {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            russh::keys::load_secret_key(&path, passphrase.as_ref().map(|p| p.expose()))
        })
    };
    match read(None).await.map_err(text)? {
        Ok(key) => return Ok(Some(key)),
        Err(russh::keys::Error::KeyIsEncrypted) => {}
        Err(e) => {
            tracing::debug!("skipping key {}: {e}", path.display());
            return Ok(None);
        }
    }
    for attempt in 0..ATTEMPTS {
        let message = format!(
            "{}Passphrase for {}",
            if attempt == 0 {
                ""
            } else {
                "That didn't work. "
            },
            path.display()
        );
        let answer = prompter
            .ask(
                PromptKind::Secret { can_save: false },
                "Key passphrase",
                &message,
            )
            .await;
        let PromptAnswer::Text { value, .. } = answer else {
            return Ok(None);
        };
        if let Ok(key) = read(Some(value)).await.map_err(text)? {
            return Ok(Some(key));
        }
    }
    Ok(None)
}

/// The keyring field recording where the saved password belongs.
const PASSWORD_TARGET: &str = "password-target";

/// `user@host:port`, where a password is sent.
fn password_target(config: &HostConfig) -> String {
    format!("{}@{}:{}", config.user, config.hostname, config.port)
}

enum Saved {
    None,
    Matches(SecretValue),
    /// Saved for another destination, so not sent.
    ForElsewhere(String),
}

/// The saved password, if it was saved for `target`.
///
/// A node's file can be changed by a `git pull` or a shared folder, and its
/// alias pointed at another server. The keyring can't be changed that way,
/// so it records where the password was used, and the password is only sent
/// there.
async fn saved_password(core: &Core, node: &Node, target: &str) -> Saved {
    let get = |field| async move { core.resolve_secret(node.id, field).await.ok().flatten() };
    let Some(password) = get("password").await else {
        return Saved::None;
    };
    match get(PASSWORD_TARGET).await {
        Some(saved) if saved.expose() == target => Saved::Matches(password),
        Some(saved) => Saved::ForElsewhere(saved.expose().to_string()),
        None => Saved::ForElsewhere("an unknown host".into()),
    }
}

/// Best effort: the user is signed in either way. The target is stored
/// first, so a failure part way never leaves a password without one.
async fn save_password(core: &Core, node: &Node, password: SecretValue, target: String) {
    for (field, value) in [
        (PASSWORD_TARGET, SecretValue::new(target)),
        ("password", password),
    ] {
        let command = Command::SetSecret {
            id: node.id,
            field: field.into(),
            value,
        };
        if let Err(e) = core.execute(command).await {
            tracing::warn!("could not save the password for {}: {e}", node.name);
            return;
        }
    }
}
