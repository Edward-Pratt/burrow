//! Desktop notifications for alerts, which the daemon sends itself over
//! D-Bus (`org.freedesktop.Notifications`), so they show while no window
//! is open. The window lists alerts instead of notifying again.
//!
//! What an alert names comes from the server. It has lost its control
//! characters already, and markup is escaped too, because some
//! notification servers read the body as markup.

use std::collections::{HashMap, HashSet};

use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use zbus::zvariant::Value;

use crate::api::{Alert, AlertId, AlertKind, Event};
use crate::engine::Core;
use crate::ssh::agent::printable;

const DESTINATION: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

/// The desktop entry the notifications belong to, for their icon.
const DESKTOP_ENTRY: &str = "dev.burrow.Burrow";

/// Sends a notification for each alert raised from now on. Without a
/// session bus, it logs that once and does nothing.
pub(crate) fn spawn(core: &Core) {
    // Subscribed before the task starts, so an alert raised while it
    // connects still shows.
    let events = core.subscribe();
    let core = core.clone();
    tokio::spawn(async move {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(e) => {
                tracing::warn!("alerts won't show as notifications: {e}");
                return;
            }
        };
        run(&core, events, &connection).await;
    });
}

/// Shows each alert once, as it is raised. Falling behind the events can
/// lose an `alert_raised`, so then it lists the alerts that hold, as
/// `list_alerts` does, and shows those it hasn't yet.
async fn run(core: &Core, mut events: broadcast::Receiver<Event>, connection: &zbus::Connection) {
    // The alerts shown that still hold, so none is shown twice.
    let mut shown: HashSet<AlertId> = HashSet::new();
    loop {
        match events.recv().await {
            Ok(Event::AlertRaised { alert }) => {
                if shown.insert(alert.id) {
                    show(core, connection, &alert).await;
                }
            }
            Ok(Event::AlertCleared { alert }) => {
                shown.remove(&alert.id);
            }
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {
                let holding = core.alerts().list();
                // A clear lost in the gap would otherwise keep its id.
                shown.retain(|id| holding.iter().any(|alert| alert.id == *id));
                for alert in holding {
                    if shown.insert(alert.id) {
                        show(core, connection, &alert).await;
                    }
                }
            }
            Err(RecvError::Closed) => return,
        }
    }
}

async fn show(core: &Core, connection: &zbus::Connection, alert: &Alert) {
    let host = core
        .node(alert.node)
        .map_or_else(|_| "A host".to_string(), |node| node.name);
    if let Err(e) = send(connection, &host, alert).await {
        tracing::warn!("could not show an alert: {e}");
    }
}

/// Shows `alert` on `host` as a notification.
async fn send(connection: &zbus::Connection, host: &str, alert: &Alert) -> zbus::Result<()> {
    let host = printable(host);
    let (summary, body) = match &alert.kind {
        AlertKind::UnitFailed { unit } => (
            format!("{host}: a service failed"),
            format!("{unit} failed."),
        ),
        AlertKind::DiskFull { mount, percent } => (
            format!("{host}: a disk is nearly full"),
            format!("{mount} is {percent:.0}% full."),
        ),
        AlertKind::MemoryLow { percent } => (
            format!("{host}: memory is low"),
            format!("{percent:.0}% of memory is available."),
        ),
        AlertKind::Unreachable { error } => (format!("{host} is not answering"), error.clone()),
        AlertKind::ContainerFailed { container, status } => (
            format!("{host}: a container stopped"),
            format!("{container}: {status}."),
        ),
        AlertKind::AppFailed { app } => (
            format!("{host}: a PM2 app failed"),
            format!("{app} errored, and PM2 stopped restarting it."),
        ),
    };
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("desktop-entry", DESKTOP_ENTRY.into());
    let actions: Vec<&str> = Vec::new();
    connection
        .call_method(
            Some(DESTINATION),
            PATH,
            Some(DESTINATION),
            "Notify",
            &(
                "Burrow",
                0u32,
                "",
                summary.as_str(),
                escape(&printable(&body)).as_str(),
                actions,
                hints,
                -1i32,
            ),
        )
        .await?;
    Ok(())
}

/// `text` with the characters notification markup gives a meaning to
/// replaced by entities.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::api::NodeId;
    use crate::engine::CoreOptions;
    use crate::ssh::alerts::Rule;

    /// What the notification server was sent: app name, summary and body.
    type Shown = Arc<Mutex<Vec<(String, String, String)>>>;

    /// A notification server that keeps what it was sent.
    struct Server {
        shown: Shown,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Server {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app_name: String,
            _replaces_id: u32,
            _app_icon: String,
            summary: String,
            body: String,
            _actions: Vec<String>,
            hints: HashMap<String, zbus::zvariant::OwnedValue>,
            _expire_timeout: i32,
        ) -> u32 {
            assert!(hints.contains_key("desktop-entry"));
            self.shown.lock().unwrap().push((app_name, summary, body));
            1
        }
    }

    /// A connection to a notification server of our own, without a bus,
    /// the server's end, and what it was sent.
    async fn notifications() -> (zbus::Connection, zbus::Connection, Shown) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let shown = Shown::default();
        let server = Server {
            shown: Arc::clone(&shown),
        };
        let guid = zbus::Guid::generate();
        let (server, client) = tokio::join!(
            zbus::connection::Builder::async_io_unix_stream(theirs)
                .server(guid)
                .unwrap()
                .p2p()
                .serve_at(PATH, server)
                .unwrap()
                .build(),
            zbus::connection::Builder::async_io_unix_stream(ours)
                .p2p()
                .build(),
        );
        (client.unwrap(), server.unwrap(), shown)
    }

    #[tokio::test]
    async fn an_alert_is_shown_with_the_server_text_escaped() {
        let (client, _server, shown) = notifications().await;

        let alert = Alert {
            id: AlertId(1),
            node: NodeId::new(),
            kind: AlertKind::UnitFailed {
                unit: "<b>x</b>&y.service".into(),
            },
            since: 0,
        };
        send(&client, "web\u{1b}", &alert).await.unwrap();
        assert_eq!(
            *shown.lock().unwrap(),
            vec![(
                "Burrow".to_string(),
                "web: a service failed".to_string(),
                "&lt;b&gt;x&lt;/b&gt;&amp;y.service failed.".to_string()
            )]
        );
    }

    /// Waits until the server has been sent `count` notifications, and
    /// returns their bodies.
    async fn bodies(shown: &Shown, count: usize) -> Vec<String> {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let bodies: Vec<String> = shown
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(_, _, body)| body.clone())
                    .collect();
                if bodies.len() >= count {
                    return bodies;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("too few notifications")
    }

    /// One task runs here, so nothing reads events between two awaits, and
    /// emitting more than the buffer holds makes the notifier fall behind.
    #[tokio::test]
    async fn an_alert_raised_while_behind_still_shows_once() {
        let (client, _server, shown) = notifications().await;
        let dir = tempfile::TempDir::new().unwrap();
        let core = Core::open(CoreOptions {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
            secrets: Arc::new(crate::secrets::MemoryStore::new()),
            ssh_config: None,
        })
        .unwrap();
        let node = NodeId::new();
        let failed = |units: &[&str]| {
            units
                .iter()
                .map(|unit| AlertKind::UnitFailed {
                    unit: unit.to_string(),
                })
                .collect()
        };
        let events = core.subscribe();
        tokio::spawn({
            let core = core.clone();
            async move { run(&core, events, &client).await }
        });

        core.set_alerts(node, Rule::Units, failed(&["a", "keep"]));
        assert_eq!(bodies(&shown, 2).await, ["a failed.", "keep failed."]);

        // Lost in the gap: a clears and is raised again, with a new id,
        // and b is raised. keep still holds and was shown already.
        core.set_alerts(node, Rule::Units, failed(&["keep"]));
        core.set_alerts(node, Rule::Units, failed(&["keep", "a", "b"]));
        for _ in 0..300 {
            core.emit(Event::ProblemsChanged {
                problems: Vec::new(),
            });
        }
        assert_eq!(
            bodies(&shown, 4).await,
            ["a failed.", "keep failed.", "a failed.", "b failed."]
        );

        // None of them shows again, and the next alert shows as usual.
        core.set_alerts(node, Rule::Units, failed(&["keep", "a", "b", "c"]));
        assert_eq!(
            bodies(&shown, 5).await,
            [
                "a failed.",
                "keep failed.",
                "a failed.",
                "b failed.",
                "c failed."
            ]
        );
    }
}
