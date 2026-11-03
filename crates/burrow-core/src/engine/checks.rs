//! Checks on what a command names: that a node is of the kind the command
//! wants, that a secret field name is plain, and a command's name for an
//! error message.

use crate::api::{Command, Node, POSTGRES_KIND, SSH_HOST_KIND, SSH_TUNNEL_KIND, VNC_KIND};
use crate::error::{Error, Result};

/// The command's `type`, as it appears in JSON.
pub(super) fn command_name(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v.get("type")?.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Fails unless `node` is of `kind`, saying what it should have been.
pub(super) fn check_kind(node: &Node, kind: &str) -> Result<()> {
    if node.kind == kind {
        return Ok(());
    }
    let wanted = match kind {
        SSH_HOST_KIND => "an SSH host".to_string(),
        SSH_TUNNEL_KIND => "an SSH tunnel".to_string(),
        POSTGRES_KIND => "a database".to_string(),
        VNC_KIND => "a desktop".to_string(),
        // No command asks for another kind today. A new one that does
        // gets a plain error rather than a panic.
        _ => format!("a {kind} node"),
    };
    Err(Error::invalid(
        "node",
        format!("{} is a {}, not {wanted}", node.name, node.kind),
    ))
}

pub(super) fn validate_field(field: &str) -> Result<()> {
    let ok = !field.is_empty()
        && field
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "secret field",
            format!("{field:?} must be lowercase letters, digits, '_' or '-'"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Node, NodeId, SSH_HOST_KIND};

    #[test]
    fn a_node_of_the_wrong_kind_is_an_error() {
        let node = Node {
            id: NodeId::new(),
            parent: None,
            kind: crate::api::FOLDER_KIND.into(),
            name: "work".into(),
            color: None,
            props: toml::Table::new(),
            secrets: Default::default(),
        };
        assert!(check_kind(&node, crate::api::FOLDER_KIND).is_ok());
        let error = check_kind(&node, SSH_HOST_KIND).unwrap_err().to_string();
        assert!(
            error.contains("work is a folder, not an SSH host"),
            "{error}"
        );
        // A kind no command asks for yet is still only an error.
        let error = check_kind(&node, "notes").unwrap_err().to_string();
        assert!(error.contains("not a notes node"), "{error}");
    }
}
