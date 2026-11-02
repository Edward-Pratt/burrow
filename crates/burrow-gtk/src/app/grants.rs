//! What the MCP bridge may do on a node, and the dialog that sets it.

use burrow_core::{
    Capability, Command, Grant, GrantShown, Node, NodeId, POSTGRES_KIND, Response, SSH_HOST_KIND,
    SSH_TUNNEL_KIND,
};
use relm4::{ComponentSender, gtk};

use crate::dialogs;
use crate::files::{self};

use super::App;
use super::daemon::execute;

use super::daemon::run;
use super::message::AppMsg;
use crate::runtime::runtime;
impl App {
    /// The grant dialog. The grant is pinned to the node as the dialog
    /// shows it, so a change made meanwhile makes saving fail instead.
    pub(super) fn edit_grant(
        &self,
        id: NodeId,
        grant: Option<Grant>,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let Some(node) = self.nodes.get(&id) else {
            return;
        };
        let nodes: Vec<Node> = self.nodes.values().cloned().collect();
        let shown = GrantShown::of(node, &nodes);
        let mut details = format!(
            "What Claude may do on {} through Burrow's MCP bridge (burrow mcp serve). \
             It holds only while these settings stay as they are:\n",
            files::printable(&node.name)
        );
        for (key, value) in &shown.props {
            details.push_str(&format!(
                "\n{} = {}",
                files::printable(key),
                files::printable(&value.to_string())
            ));
        }
        if let Some(via) = &shown.via {
            details.push_str(&format!("\n\nThrough {}:", files::printable(&via.name)));
            for (key, value) in &via.props {
                details.push_str(&format!(
                    "\n{} = {}",
                    files::printable(key),
                    files::printable(&value.to_string())
                ));
            }
        }
        let granted = match &grant {
            Some(grant) if grant.stale => {
                details.push_str(
                    "\n\nThese changed since access was given, so nothing is allowed \
                     until you save again.",
                );
                Default::default()
            }
            Some(grant) => grant.capabilities.clone(),
            None => Default::default(),
        };
        if node.kind == POSTGRES_KIND {
            details.push_str(
                "\n\nRead-only SQL is refused for a superuser, or a role that may write \
                 files or run programs on the server, since Burrow can't keep those \
                 read-only. For full safety, connect as a role that can only read. \
                 Allowing SQL that writes as a superuser also lets Claude run \
                 programs on the database server.",
            );
        }
        if node.kind == SSH_HOST_KIND {
            details.push_str(&format!(
                "\n\n“{}” amounts to running commands, since it can change \
                 ~/.ssh/authorized_keys or shell start-up files. Give it as carefully \
                 as “{}”.",
                capability_label(Capability::FilesWrite),
                capability_label(Capability::Exec),
            ));
        }
        let offered = capabilities(&node.kind);
        let options = offered
            .iter()
            .map(|c| (capability_label(*c).to_string(), granted.contains(c)))
            .collect();
        let (core, sender) = (self.core.clone(), sender.clone());
        dialogs::choose(window, "Claude access", &details, options, move |ticked| {
            let capabilities = offered
                .iter()
                .zip(ticked)
                .filter_map(|(c, on)| on.then_some(*c))
                .collect();
            let command = Command::SetGrant {
                node: id,
                capabilities,
                expect: Some(shown),
            };
            execute(&core, &sender, command);
        });
    }

    /// Looks up the node's grant, for [`AppMsg::GrantLoaded`].
    pub(super) fn load_grant(&self, node: NodeId, sender: &ComponentSender<Self>) {
        let (core, input) = (self.core.clone(), sender.input_sender().clone());
        runtime().spawn(async move {
            let message = match run(&core, Command::ListGrants).await {
                Ok(Response::Grants { grants }) => AppMsg::GrantLoaded {
                    node,
                    grant: grants.into_iter().find(|g| g.node == node),
                },
                Ok(_) => return,
                Err(error) => AppMsg::Failed(error),
            };
            let _ = input.send(message);
        });
    }
}

/// What the MCP bridge may be granted on a node of `kind`.
pub(super) fn capabilities(kind: &str) -> Vec<Capability> {
    match kind {
        POSTGRES_KIND => vec![Capability::SqlRead, Capability::SqlWrite],
        SSH_HOST_KIND => vec![
            Capability::Exec,
            Capability::FilesRead,
            Capability::FilesWrite,
            Capability::MonitorRead,
        ],
        SSH_TUNNEL_KIND => vec![Capability::Tunnel],
        _ => Vec::new(),
    }
}

pub(super) fn capability_label(capability: Capability) -> &'static str {
    match capability {
        Capability::SqlRead => "Run read-only SQL",
        Capability::SqlWrite => "Run SQL that writes",
        Capability::Exec => "Run commands",
        Capability::FilesRead => "List and read files",
        Capability::FilesWrite => "Create, change and delete files",
        Capability::Tunnel => "Start and stop the tunnel",
        Capability::MonitorRead => "Read system stats and services",
    }
}
