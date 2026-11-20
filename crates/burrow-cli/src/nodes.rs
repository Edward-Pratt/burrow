//! The nodes in the sidebar: listing them, and finding the one a command
//! names.

use std::collections::HashMap;
use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::{Command, Node, NodeId, Response};

use crate::text::clean;
use crate::{Result, unexpected};

pub async fn nodes(client: &Client) -> Result<Vec<Node>> {
    match client.execute(Command::ListNodes).await? {
        Response::Nodes { nodes } => Ok(nodes),
        other => Err(unexpected(&other)),
    }
}

/// The node named `arg`, or with `arg` as its id.
pub async fn find(client: &Client, arg: &str) -> Result<Node> {
    let mut found: Vec<Node> = nodes(client)
        .await?
        .into_iter()
        .filter(|n| n.id.to_string() == arg || n.name == arg)
        .collect();
    match found.len() {
        0 => Err(format!("no node is called {arg:?}").into()),
        1 => Ok(found.remove(0)),
        _ => {
            let ids: Vec<String> = found.iter().map(|n| n.id.to_string()).collect();
            Err(format!(
                "{} nodes are called {arg:?}; name one by id: {}",
                found.len(),
                ids.join(", ")
            )
            .into())
        }
    }
}

pub async fn ls(client: &Client, json: bool) -> Result<ExitCode> {
    let nodes = nodes(client).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut children: HashMap<Option<NodeId>, Vec<&Node>> = HashMap::new();
    for node in &nodes {
        children.entry(node.parent).or_default().push(node);
    }
    fn print(children: &HashMap<Option<NodeId>, Vec<&Node>>, parent: Option<NodeId>, depth: usize) {
        for node in children.get(&parent).into_iter().flatten() {
            let indent = "  ".repeat(depth);
            println!("{indent}{}\t{}\t{}", clean(&node.name), node.kind, node.id);
            print(children, Some(node.id), depth + 1);
        }
    }
    print(&children, None, 0);
    Ok(ExitCode::SUCCESS)
}
