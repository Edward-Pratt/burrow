//! The connection to burrowd: running commands, loading its state, listening for its events and reconnecting.

use std::time::Duration;

use burrow_core::ipc::{self, Client, Role};
use burrow_core::{Command, Event, NodeId, Response, SessionId, SessionState};
use relm4::{ComponentSender, gtk};
use tokio::sync::broadcast::error::RecvError;

use crate::files::{self};
use crate::runtime::runtime;
use crate::session;

use super::App;
use super::message::AppMsg;

/// How long to wait between attempts to reach a daemon that went away.
pub(super) const RECONNECT_DELAY: Duration = Duration::from_secs(2);

impl App {
    /// Switches to a new daemon after the old one went away. What ran in
    /// the old one is over, and the new one is loaded from scratch.
    pub(super) fn reconnected(
        &mut self,
        client: Client,
        window: &gtk::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        self.core = client;
        self.reconnecting = false;
        // Why reconnecting failed is over now; the list still has it.
        self.error = None;
        if let Some(error) = stale_daemon(&self.core) {
            self.fail(error);
        }
        // Their sessions are gone, and no answer could reach them.
        for (_, dialog) in self.prompts.get_mut().drain() {
            dialog.dismiss();
        }
        // Sessions ended with the daemon that ran them. Tunnels are settled
        // by the reload; the rest are only known here.
        let ended: Vec<(NodeId, SessionId)> = (self.tabs.iter())
            .filter_map(|(key, tab)| match tab.panel.session() {
                Some(session::State::Running(session)) => Some((key.node()?, session)),
                _ => None,
            })
            .collect();
        for (node, session) in ended {
            let state = SessionState::Closed {
                error: Some("Burrow's core stopped".to_string()),
            };
            let event = Event::SessionChanged {
                session,
                node,
                state,
            };
            self.session_event(event, window, sender);
        }
        listen(&self.core, sender);
        load(&self.core, sender);
        sender.input(AppMsg::Notice("Connected to burrowd again".to_string()));
    }
}

/// Runs a command on the core's runtime. Successful results arrive as
/// events, so only failures are reported back here.
pub(super) fn execute(core: &Client, sender: &ComponentSender<App>, command: Command) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        if let Err(error) = run(&core, command).await {
            let _ = input.send(AppMsg::Failed(error));
        }
    });
}

/// Runs a command, naming it in the error, so that "permission denied"
/// says what was denied.
pub(super) async fn run(core: &Client, command: Command) -> Result<Response, String> {
    let name = command_name(&command);
    core.execute(command)
        .await
        .map_err(|e| format!("{name}: {e}"))
}

/// The command's `type` as the API names it, such as `start_tunnel`. It is
/// read from the start of the command's `Debug` text, and the writer stops
/// there, so a command carrying a file's bytes isn't formatted whole.
pub(super) fn command_name(command: &Command) -> String {
    use std::fmt::Write;

    /// serde's `snake_case` of the variant name.
    struct Name(String);
    impl Write for Name {
        fn write_str(&mut self, text: &str) -> std::fmt::Result {
            for c in text.chars() {
                if !c.is_ascii_alphanumeric() {
                    return Err(std::fmt::Error);
                }
                if c.is_ascii_uppercase() && !self.0.is_empty() {
                    self.0.push('_');
                }
                self.0.push(c.to_ascii_lowercase());
            }
            Ok(())
        }
    }
    let mut name = Name(String::new());
    // Fails, on purpose, at the first character after the name.
    let _ = write!(name, "{command:?}");
    name.0
}

/// Loads the full node list, problems, addons, sessions and autostart
/// tunnels.
pub(super) fn load(core: &Client, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let result = async {
            let nodes = match run(&core, Command::ListNodes).await? {
                Response::Nodes { nodes } => nodes,
                _ => Vec::new(),
            };
            let problems = match run(&core, Command::ListProblems).await? {
                Response::Problems { problems } => problems,
                _ => Vec::new(),
            };
            let addons = match run(&core, Command::ListAddons).await? {
                Response::Addons { addons } => addons,
                _ => Vec::new(),
            };
            let sessions = match run(&core, Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                _ => Vec::new(),
            };
            let autostart = match run(&core, Command::ListAutostart).await? {
                Response::Autostart { nodes } => nodes,
                _ => Vec::new(),
            };
            let monitoring = match run(&core, Command::ListMonitoring).await? {
                Response::Monitoring { nodes } => nodes,
                _ => Vec::new(),
            };
            let alerts = match run(&core, Command::ListAlerts).await? {
                Response::Alerts { alerts } => alerts,
                _ => Vec::new(),
            };
            Ok::<_, String>(AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
                monitoring,
                alerts,
            })
        }
        .await;
        let _ = input.send(result.unwrap_or_else(AppMsg::Failed));
    });
}

/// Looks up every running session, for [`AppMsg::SessionsListed`].
pub(super) fn list_sessions(core: &Client, sender: &ComponentSender<App>) {
    let (core, input) = (core.clone(), sender.input_sender().clone());
    runtime().spawn(async move {
        let sessions = match core.execute(Command::ListSessions).await {
            Ok(Response::Sessions { sessions }) => sessions,
            _ => Vec::new(),
        };
        let _ = input.send(AppMsg::SessionsListed(sessions));
    });
}

/// Forwards core events to the window until it closes. If the daemon goes
/// away, connects again, which starts a new one.
pub(super) fn listen(core: &Client, sender: &ComponentSender<App>) {
    let mut events = core.subscribe();
    let input = sender.input_sender().clone();
    runtime().spawn(async move {
        loop {
            let message = match events.recv().await {
                Ok(event) => AppMsg::CoreEvent(event),
                Err(RecvError::Lagged(_)) => AppMsg::Resync,
                Err(RecvError::Closed) => break,
            };
            if input.send(message).is_err() {
                return; // The window is gone.
            }
        }
        if input.send(AppMsg::Disconnected).is_err() {
            return;
        }
        let mut last = String::new();
        loop {
            match ipc::connect(Role::Gui).await {
                Ok(client) => {
                    let _ = input.send(AppMsg::Reconnected(client));
                    return;
                }
                Err(e) => {
                    let error = format!("could not reach burrowd: {e}");
                    eprintln!("burrow: {error}");
                    // Only a new reason: one every two seconds would push
                    // everything else out of the errors list.
                    if error != last {
                        if input.send(AppMsg::Failed(error.clone())).is_err() {
                            return; // The window is gone.
                        }
                        last = error;
                    }
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            }
        }
    });
}

/// What to say when the daemon is from another build: one that kept
/// running through an upgrade because tunnels or terminals run in it.
pub(super) fn stale_daemon(core: &Client) -> Option<String> {
    let build = &core.info().build;
    (*build != ipc::BUILD).then(|| {
        format!(
            "Burrow's core ({}) is from before an update. Stop what runs in it, \
             then run \"burrow daemon stop\".",
            files::printable(build)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_core::{Command, NodeId, SessionId};

    #[test]
    fn commands_are_named_as_the_api_names_them() {
        assert_eq!(command_name(&Command::ListNodes), "list_nodes");
        let node = NodeId::new();
        assert_eq!(command_name(&Command::StartTunnel { node }), "start_tunnel");
        let close = Command::CloseSession {
            session: SessionId(7),
        };
        assert_eq!(command_name(&close), "close_session");
    }
}
