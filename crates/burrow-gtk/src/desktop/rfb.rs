//! The RFB side of a desktop: vnc-rs over the core's session.

use std::time::Duration;

use burrow_core::ipc::Client;
use burrow_core::{Command, NodeId, SessionId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use vnc::{PixelFormat, VncConnector, VncEncoding, X11Event};

use super::Msg;
use crate::app::AppMsg;

/// How often the server is asked for what changed.
const REFRESH: Duration = Duration::from_millis(50);

/// Speaks RFB over the core's session until either side ends. vnc-rs reads
/// and writes one end of a pipe; the core's output goes into the other,
/// and what comes out of it goes to the core.
pub(super) async fn drive(
    core: &Client,
    session: SessionId,
    mut events: mpsc::UnboundedReceiver<X11Event>,
    sender: &relm4::Sender<AppMsg>,
    node: NodeId,
) -> Result<(), String> {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (mut from_client, mut to_client) = tokio::io::split(theirs);
    let mut output = core.take_output(session).await.map_err(|e| e.to_string())?;
    // Two tasks, so neither direction waits on the other.
    tokio::spawn(async move {
        while let Some(data) = output.recv().await {
            if to_client.write_all(&data).await.is_err() {
                break;
            }
        }
    });
    let writer = core.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0; 64 << 10];
        while let Ok(n @ 1..) = from_client.read(&mut buffer).await {
            let data = buffer[..n].to_vec();
            if writer
                .execute(Command::WriteTerminal { session, data })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let text = |e: vnc::VncError| e.to_string();
    let vnc = VncConnector::new(ours)
        // The core has signed in, so the stand-in server asks for nothing.
        .set_auth_method(async { Ok(String::new()) })
        .add_encoding(VncEncoding::Zrle)
        .add_encoding(VncEncoding::CopyRect)
        .add_encoding(VncEncoding::Raw)
        .add_encoding(VncEncoding::DesktopSizePseudo)
        .allow_shared(true)
        .set_pixel_format(PixelFormat::bgra())
        .build()
        .map_err(text)?
        .try_start()
        .await
        .map_err(text)?
        .finish()
        .map_err(text)?;
    vnc.input(X11Event::FullRefresh).await.map_err(text)?;
    let mut refresh = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            event = vnc.recv_event() => {
                let msg = Msg::Event(event.map_err(text)?);
                if sender.send(AppMsg::Desktop { node, msg }).is_err() {
                    return Ok(());
                }
            }
            Some(event) = events.recv() => vnc.input(event).await.map_err(text)?,
            _ = refresh.tick() => vnc.input(X11Event::Refresh).await.map_err(text)?,
        }
    }
}
