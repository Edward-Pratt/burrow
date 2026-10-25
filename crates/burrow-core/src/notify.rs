//! Desktop notifications for alerts, which the daemon sends itself over
//! D-Bus (`org.freedesktop.Notifications`), so they show while no window
//! is open. The window lists alerts instead of notifying again.
//!
//! What an alert names comes from the server. It has lost its control
//! characters already, and markup is escaped too, because some
//! notification servers read the body as markup.

use std::collections::HashMap;

use tokio::sync::broadcast::error::RecvError;
use zbus::zvariant::Value;

use crate::api::{Alert, AlertKind, Event};
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
    let mut events = core.subscribe();
    let core = core.clone();
    tokio::spawn(async move {
        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(e) => {
                tracing::warn!("alerts won't show as notifications: {e}");
                return;
            }
        };
        loop {
            match events.recv().await {
                Ok(Event::AlertRaised { alert }) => {
                    let host = core
                        .node(alert.node)
                        .map_or_else(|_| "A host".to_string(), |node| node.name);
                    if let Err(e) = send(&connection, &host, &alert).await {
                        tracing::warn!("could not show an alert: {e}");
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => return,
            }
        }
    });
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
    use crate::api::{AlertId, NodeId};

    /// A notification server that keeps what it was sent.
    struct Server {
        shown: Arc<Mutex<Vec<(String, String, String)>>>,
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

    #[tokio::test]
    async fn an_alert_is_shown_with_the_server_text_escaped() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let shown = Arc::default();
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
        let (_server, client) = (server.unwrap(), client.unwrap());

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
}
