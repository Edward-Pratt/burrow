//! Signing in to a host, in the order OpenSSH tries: agent keys, key
//! files, a password, then keyboard-interactive.

use std::path::Path;
use std::sync::Arc;

use russh::client::{AuthResult, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKey};
use russh::{MethodKind, MethodSet};

use super::{Client, Result, text};
use crate::api::{Node, PromptAnswer, PromptKind, SecretValue};
use crate::engine::Core;
use crate::password::{Saved, save_password, saved_password};
use crate::ssh::Prompter;
use crate::ssh::config::HostConfig;

/// Wrong passwords or passphrases allowed before giving up, as OpenSSH.
const ATTEMPTS: usize = 3;

/// Signs in the way OpenSSH would try: agent keys, then key files, then a
/// password, then keyboard-interactive.
pub(super) async fn authenticate(
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

/// `user@host:port`, where a password is sent.
fn password_target(config: &HostConfig) -> String {
    format!("{}@{}:{}", config.user, config.hostname, config.port)
}
