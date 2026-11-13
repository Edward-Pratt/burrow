//! Signing in with the SSH agent's keys, and how long the agent may take.
//!
//! An agent can stop to ask before it signs: gcr-ssh-agent and KeePassXC
//! for a key's passphrase, `ssh-add -c` keys and 1Password for leave to
//! use the key. Its dialog is on the screen of the computer Burrow runs on,
//! not in Burrow, so Burrow can't show or answer it. When nobody answers,
//! signing in stops and says so, rather than wait as long as the session
//! allows.

use std::path::Path;
use std::time::Duration;

use russh::client::{AuthResult, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PublicKey};
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::{Client, Result};
use crate::ssh::Prompter;

/// How long the agent may take to list its keys, which asks nobody.
const LIST: Duration = Duration::from_secs(10);

/// How long a key may take to sign when someone is at Burrow, and so
/// likely at the screen where the agent asks.
const SIGN: Duration = Duration::from_secs(120);

/// The same for a session nobody at Burrow watches, such as the MCP
/// bridge's: the user is likely elsewhere, so it gives up sooner and says
/// why, well inside the bridge's own waits.
const SIGN_UNATTENDED: Duration = Duration::from_secs(15);

pub(super) type Agent = AgentClient<UnixStream>;

/// The agent at `socket` and the keys it holds, or `None` when there is no
/// agent there or it doesn't answer.
pub(super) async fn keys(socket: &Path) -> Option<(Agent, Vec<PublicKey>)> {
    let mut agent = match AgentClient::connect_uds(socket).await {
        Ok(agent) => agent,
        Err(e) => {
            tracing::debug!("no SSH agent at {}: {e}", socket.display());
            return None;
        }
    };
    let identities = match timeout(LIST, agent.request_identities()).await {
        Ok(identities) => identities.unwrap_or_default(),
        Err(_) => {
            tracing::warn!("the SSH agent at {} didn't list its keys", socket.display());
            return None;
        }
    };
    let keys = identities
        .into_iter()
        .filter_map(|identity| match identity {
            AgentIdentity::PublicKey { key, .. } => Some(key),
            AgentIdentity::Certificate { .. } => None,
        })
        .collect();
    Some((agent, keys))
}

/// Offers `key` and has the agent sign with it. The inner error is the
/// agent declining, after which other keys may still do. The outer one is
/// the agent not answering, which ends signing in: it is most likely
/// waiting on a dialog, and trying a key file next would only ask for that
/// key's passphrase in Burrow as well.
pub(super) async fn sign(
    handle: &mut Handle<Client>,
    user: &str,
    key: &PublicKey,
    hash: Option<HashAlg>,
    agent: &mut Agent,
    prompter: &Prompter,
) -> Result<std::result::Result<AuthResult, String>> {
    let limit = if prompter.attended() {
        SIGN
    } else {
        SIGN_UNATTENDED
    };
    let signed = handle.authenticate_publickey_with(user, key.clone(), hash, agent);
    match timeout(limit, signed).await {
        Ok(signed) => Ok(signed.map_err(|e| e.to_string())),
        Err(_) => Err(format!(
            "the SSH agent didn't sign with the key {} within {} s. It may be asking for \
             the key's passphrase, or whether to allow the key, in a dialog on this \
             computer's screen: answer it there, then try again",
            key.fingerprint(HashAlg::Sha256),
            limit.as_secs()
        )),
    }
}
