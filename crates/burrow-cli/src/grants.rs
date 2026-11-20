//! `burrow mcp ls`, `grant` and `revoke`: what an AI assistant may do on
//! each node. The daemon pins a grant to what the user was shown here.

use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::{Capability, Command, Grant, GrantShown, Response};

use crate::args::McpCmd;
use crate::nodes::{find, nodes};
use crate::text::clean;
use crate::{Result, unexpected};

async fn grants(client: &Client) -> Result<Vec<Grant>> {
    match client.execute(Command::ListGrants).await? {
        Response::Grants { grants } => Ok(grants),
        other => Err(unexpected(&other)),
    }
}

pub async fn run(client: &Client, action: McpCmd) -> Result<ExitCode> {
    match action {
        McpCmd::Serve => unreachable!("handled in run"),
        McpCmd::Ls => {
            let nodes = nodes(client).await?;
            for grant in grants(client).await? {
                let name = nodes
                    .iter()
                    .find(|n| n.id == grant.node)
                    .map(|n| clean(&n.name))
                    .unwrap_or_default();
                let capabilities: Vec<String> =
                    grant.capabilities.iter().map(|c| c.to_string()).collect();
                let stale = if grant.stale {
                    "\tstale: changed since granted"
                } else {
                    ""
                };
                println!("{name}\t{}\t{}{stale}", capabilities.join(","), grant.node);
            }
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Grant {
            node,
            capabilities,
            yes,
        } => {
            let nodes = nodes(client).await?;
            let node = find(client, &node).await?;
            let shown = GrantShown::of(&node, &nodes);
            eprint!("{}", describe(&node.kind, &shown));
            let mut all: std::collections::BTreeSet<Capability> = grants(client)
                .await?
                .into_iter()
                .find(|g| g.node == node.id && !g.stale)
                .map(|g| g.capabilities)
                .unwrap_or_default();
            all.extend(capabilities);
            let list: Vec<String> = all.iter().map(|c| c.to_string()).collect();
            if !yes {
                if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    return Err("not asking without a terminal; pass --yes".into());
                }
                eprint!("Let the assistant use {} here? [y/N] ", list.join(", "));
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    return Ok(ExitCode::FAILURE);
                }
            }
            let command = Command::SetGrant {
                node: node.id,
                capabilities: all,
                expect: Some(shown),
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Revoke { node, capabilities } => {
            let node = find(client, &node).await?;
            let grant = grants(client)
                .await?
                .into_iter()
                .find(|g| g.node == node.id);
            let Some(grant) = grant else {
                return Ok(ExitCode::SUCCESS);
            };
            let mut left = grant.capabilities;
            left.retain(|c| !capabilities.is_empty() && !capabilities.contains(c));
            // What is left gets pinned again, to the node as it is now. A
            // stale grant changed after the user saw it, so it isn't kept.
            let expect = match left.is_empty() {
                true => None,
                false if grant.stale => {
                    return Err(format!(
                        "{} changed since it was granted; revoke it all, then grant it again",
                        clean(&node.name)
                    )
                    .into());
                }
                false => Some(GrantShown::of(&node, &nodes(client).await?)),
            };
            let command = Command::SetGrant {
                node: node.id,
                capabilities: left,
                expect,
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A node's settings, as shown before granting, with anything that came
/// from a file escaped.
fn describe(kind: &str, shown: &GrantShown) -> String {
    let mut text = format!("{} ({kind})\n", clean(&shown.name));
    for (key, value) in &shown.props {
        text.push_str(&format!(
            "  {} = {}\n",
            clean(key),
            clean(&value.to_string())
        ));
    }
    if let Some(via) = &shown.via {
        text.push_str(&format!("  through {}\n", clean(&via.name)));
        for (key, value) in &via.props {
            text.push_str(&format!(
                "    {} = {}\n",
                clean(key),
                clean(&value.to_string())
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_grant_shows_is_escaped_keys_too() {
        let mut props = toml::Table::new();
        props.insert("alias\u{1b}[2K".into(), "web\u{1b}[2K".into());
        let shown = GrantShown {
            name: "web".into(),
            props,
            via: None,
        };
        let text = describe("ssh-host", &shown);
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }
}
