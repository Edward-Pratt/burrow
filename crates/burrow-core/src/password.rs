//! Saved passwords, pinned to where they were used.
//!
//! A node's file can be changed by a `git pull` or a shared folder, and
//! pointed at another server. The keyring can't be changed that way, so it
//! records where each password was used next to the password itself, and
//! the password is only sent there. Each connector describes its own
//! target, including anything whose change should make Burrow ask again.

use crate::api::{Command, Node, SecretValue};
use crate::engine::Core;

/// The keyring field recording where the saved password belongs.
const PASSWORD_TARGET: &str = "password-target";

pub(crate) enum Saved {
    None,
    Matches(SecretValue),
    /// Saved for another destination, so not sent.
    ForElsewhere(String),
}

/// The node's saved password, if it was saved for `target`.
pub(crate) async fn saved_password(core: &Core, node: &Node, target: &str) -> Saved {
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
pub(crate) async fn save_password(core: &Core, node: &Node, password: SecretValue, target: String) {
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
