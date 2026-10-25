//! SSH tunnels: a local port forwarded through an SSH host, either to one
//! fixed destination (`ssh -L`) or wherever a SOCKS client asks
//! (`ssh -D`).
//!
//! A tunnel node's file can arrive through a shared folder or a `git pull`,
//! so its props are checked before anything is bound. It only listens on a
//! loopback address, and it never starts by itself unless the user said so
//! on this computer ([`Autostart`]).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use data_encoding::HEXLOWER;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::connection::{Connection, alias};
use super::{Input, Prompter, socks};
use crate::api::{Event, Node, NodeId, SSH_HOST_KIND, SessionState};
use crate::engine::Core;
use crate::error::{Error, Result};
use crate::net::check_host;
use crate::pins::{Lookup, Pinned};

/// What a tunnel node's props describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    /// The SSH host node it goes through.
    pub via: NodeId,
    pub listen: SocketAddr,
    pub forward: Forward,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Forward {
    /// `ssh -L`: every connection goes to `host:port`, as seen from the SSH
    /// host.
    Local { host: String, port: u16 },
    /// `ssh -D`: a SOCKS5 proxy.
    Dynamic,
}

impl Spec {
    /// Reads and checks a tunnel node's props:
    ///
    /// ```toml
    /// via = "<id of an ssh-host node>"
    /// type = "local"          # or "dynamic"
    /// bind = "127.0.0.1"      # optional; must be a loopback address
    /// port = 5433
    /// target_host = "db.internal"   # "local" only
    /// target_port = 5432            # "local" only
    /// ```
    pub fn from_node(node: &Node) -> std::result::Result<Spec, String> {
        let props = &node.props;
        let string = |key: &str| props.get(key).and_then(|v| v.as_str());
        let port = |key: &str| -> std::result::Result<u16, String> {
            match props.get(key) {
                None => Err(format!("{key} is missing")),
                Some(value) => value
                    .as_integer()
                    .and_then(|n| u16::try_from(n).ok())
                    .filter(|&n| n != 0)
                    .ok_or_else(|| format!("{key} must be a port number from 1 to 65535")),
            }
        };

        let via = string("via")
            .ok_or("via is missing: it names the SSH host to go through")?
            .parse()
            .map(NodeId)
            .map_err(|_| "via must be the id of an SSH host".to_string())?;
        let bind = match string("bind") {
            None => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Some("localhost") => IpAddr::V4(Ipv4Addr::LOCALHOST),
            Some(bind) => bind
                .parse()
                .map_err(|_| format!("bind {bind:?} is not an IP address"))?,
        };
        // Anything else would let other machines on the network use the
        // tunnel, and a node file from a shared folder must not decide that.
        if !bind.is_loopback() {
            return Err(format!(
                "bind {bind} is not a loopback address; Burrow only listens on this computer"
            ));
        }
        let listen = SocketAddr::new(bind, port("port")?);
        let forward = match string("type") {
            Some("local") => {
                let host = string("target_host").ok_or("target_host is missing")?;
                check_host("target_host", host)?;
                Forward::Local {
                    host: host.to_string(),
                    port: port("target_port")?,
                }
            }
            Some("dynamic") => Forward::Dynamic,
            Some(other) => {
                return Err(format!("type {other:?} is not \"local\" or \"dynamic\""));
            }
            None => return Err("type is missing: \"local\" or \"dynamic\"".into()),
        };
        Ok(Spec {
            via,
            listen,
            forward,
        })
    }

    /// What an autostart consent covers: everything that decides where the
    /// tunnel goes, including the host's alias.
    fn fingerprint(&self, alias: &str) -> String {
        let forward = match &self.forward {
            Forward::Local { host, port } => format!("local {host} {port}"),
            Forward::Dynamic => "dynamic".into(),
        };
        let text = format!("{}\n{alias}\n{}\n{forward}", self.via, self.listen);
        HEXLOWER.encode(&Sha256::digest(text.as_bytes()))
    }
}

/// The tunnel's SSH host node, which must exist and be an SSH host.
fn host_node(lookup: &impl Lookup, spec: &Spec) -> std::result::Result<Node, String> {
    let host = lookup(spec.via).ok_or("the SSH host this tunnel goes through no longer exists")?;
    if host.kind != SSH_HOST_KIND {
        return Err(format!("{} is a {}, not an SSH host", host.name, host.kind));
    }
    Ok(host)
}

/// Runs a tunnel until it is closed or its connection ends.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    mut input: mpsc::UnboundedReceiver<Input>,
    prompter: &Prompter,
) -> std::result::Result<(), String> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let spec = Spec::from_node(node)?;
    let host = host_node(&|id| core.node(id).ok(), &spec)?;

    // Bound first, so a port that is taken fails before any sign-in.
    step(format!("Listening on {}", spec.listen));
    let listener = TcpListener::bind(spec.listen)
        .await
        .map_err(|e| format!("could not listen on {}: {e}", spec.listen))?;

    let setup = core.connections().get(core, &host, prompter, &step);
    let closed = async {
        while let Some(input) = input.recv().await {
            if let Input::Close = input {
                return;
            }
        }
    };
    let connection = tokio::select! {
        connection = setup => connection?,
        () = closed => return Ok(()),
    };
    core.emit(Event::SessionChanged {
        session: prompter.session,
        node: node.id,
        state: SessionState::Connected,
    });

    // Dropped on the way out, which ends every forwarded connection.
    let mut forwards = JoinSet::new();
    let spec = Arc::new(spec);
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((socket, peer)) => {
                    let (connection, spec) = (connection.clone(), spec.clone());
                    forwards.spawn(async move {
                        if let Err(e) = forward(&connection, &spec, socket, peer).await {
                            tracing::debug!("tunnel connection from {peer} ended: {e}");
                        }
                    });
                }
                Err(e) => tracing::debug!("tunnel accept failed: {e}"),
            },
            // Reap finished forwards so the set doesn't grow.
            Some(_) = forwards.join_next(), if !forwards.is_empty() => {}
            reason = connection.closed() => return Err(reason),
            message = input.recv() => match message {
                Some(Input::Close) | None => return Ok(()),
                Some(_) => {}
            },
        }
    }
}

/// Carries one accepted connection through the SSH host.
async fn forward(
    connection: &Connection,
    spec: &Spec,
    mut socket: TcpStream,
    peer: SocketAddr,
) -> std::io::Result<()> {
    let _ = socket.set_nodelay(true);
    let (host, port, socks) = match &spec.forward {
        Forward::Local { host, port } => (host.clone(), *port, false),
        Forward::Dynamic => {
            let (host, port) = socks::accept(&mut socket).await?;
            (host, port, true)
        }
    };
    let opened = connection
        .handle()
        .channel_open_direct_tcpip(
            host.as_str(),
            port.into(),
            peer.ip().to_string(),
            peer.port().into(),
        )
        .await;
    let channel = match opened {
        Ok(channel) => channel,
        Err(e) => {
            if socks {
                socks::reply(&mut socket, socks::REFUSED).await?;
            }
            return Err(std::io::Error::other(format!(
                "the SSH host could not open {host}:{port}: {e}"
            )));
        }
    };
    if socks {
        socks::reply(&mut socket, socks::SUCCEEDED).await?;
    }
    let mut stream = channel.into_stream();
    tokio::io::copy_bidirectional(&mut socket, &mut stream).await?;
    let _ = socket.shutdown().await;
    Ok(())
}

/// Which tunnels start with Burrow. Kept in the state folder, apart from
/// the tunnel files, so a tunnel that arrives with a `git pull` never starts
/// unasked. Each consent is pinned to the tunnel's settings and its host's
/// alias, so changing where it goes withdraws it. The state file's
/// `[tunnels]` table holds the fingerprint each was enabled for.
pub(crate) struct Autostart(Pinned<NodeId, String>);

impl Autostart {
    /// Reads the state file. A broken one is logged and treated as empty
    /// until the next change rewrites it: nothing starts by itself.
    pub fn load(path: PathBuf) -> Autostart {
        Autostart(Pinned::load_or_warn(path, "tunnels"))
    }

    /// The tunnels whose consent still matches their settings.
    pub fn enabled(&self, lookup: &impl Lookup) -> Vec<NodeId> {
        self.0
            .entries()
            .into_iter()
            .filter(|(id, fingerprint)| {
                current_fingerprint(lookup, *id).as_ref() == Some(fingerprint)
            })
            .map(|(id, _)| id)
            .collect()
    }

    pub fn set(&self, lookup: &impl Lookup, node: &Node, enabled: bool) -> Result<()> {
        if enabled {
            let fingerprint = current_fingerprint(lookup, node.id).ok_or_else(|| {
                let reason = Spec::from_node(node)
                    .and_then(|spec| host_node(lookup, &spec).map(|_| ()))
                    .err()
                    .unwrap_or_else(|| "its settings are incomplete".into());
                Error::invalid("tunnel", reason)
            })?;
            self.0.insert(node.id, fingerprint)
        } else {
            self.0.remove(&node.id)
        }
    }

    /// Drops consents for tunnels that no longer exist.
    pub fn forget(&self, nodes: &[NodeId]) {
        self.0.forget(nodes);
    }
}

/// The fingerprint of a tunnel node as it is now, if it is a valid tunnel
/// through an existing host.
fn current_fingerprint(lookup: &impl Lookup, id: NodeId) -> Option<String> {
    let node = lookup(id)?;
    let spec = Spec::from_node(&node).ok()?;
    let host = host_node(lookup, &spec).ok()?;
    Some(spec.fingerprint(alias(&host)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(props: &str) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            kind: "ssh-tunnel".into(),
            name: "t".into(),
            color: None,
            props: toml::from_str(props).unwrap(),
            secrets: Default::default(),
        }
    }

    const VIA: &str = "via = \"6f1c2d3e-0000-4000-8000-000000000001\"\n";

    #[test]
    fn reads_a_local_forward() {
        let spec = Spec::from_node(&node(&format!(
            "{VIA}type = \"local\"\nport = 5433\ntarget_host = \"db\"\ntarget_port = 5432"
        )))
        .unwrap();
        assert_eq!(spec.listen, "127.0.0.1:5433".parse().unwrap());
        assert_eq!(
            spec.forward,
            Forward::Local {
                host: "db".into(),
                port: 5432
            }
        );
    }

    #[test]
    fn only_listens_on_loopback() {
        let props = |bind: &str| format!("{VIA}type = \"dynamic\"\nport = 1080\nbind = \"{bind}\"");
        assert!(Spec::from_node(&node(&props("::1"))).is_ok());
        assert!(Spec::from_node(&node(&props("127.0.0.2"))).is_ok());
        for bind in ["0.0.0.0", "192.168.1.5", "::", "example.com"] {
            assert!(Spec::from_node(&node(&props(bind))).is_err(), "{bind}");
        }
    }

    #[test]
    fn refuses_bad_ports_and_hosts() {
        for props in [
            "type = \"dynamic\"\nport = 0",
            "type = \"dynamic\"\nport = 70000",
            "type = \"dynamic\"\nport = \"80\"",
            "type = \"local\"\nport = 1\ntarget_host = \"-oProxyCommand=x\"\ntarget_port = 1",
            "type = \"local\"\nport = 1\ntarget_host = \"a b\"\ntarget_port = 1",
            "type = \"remote\"\nport = 1",
        ] {
            assert!(
                Spec::from_node(&node(&format!("{VIA}{props}"))).is_err(),
                "{props}"
            );
        }
        assert!(Spec::from_node(&node("type = \"dynamic\"\nport = 1")).is_err());
    }

    #[test]
    fn the_fingerprint_covers_where_it_goes() {
        let spec = |target: &str| {
            Spec::from_node(&node(&format!(
                "{VIA}type = \"local\"\nport = 5433\ntarget_host = \"{target}\"\ntarget_port = 5432"
            )))
            .unwrap()
        };
        assert_eq!(spec("db").fingerprint("web"), spec("db").fingerprint("web"));
        assert_ne!(
            spec("db").fingerprint("web"),
            spec("other").fingerprint("web")
        );
        assert_ne!(
            spec("db").fingerprint("web"),
            spec("db").fingerprint("evil")
        );
    }

    /// The file exactly as Burrow wrote it before the stores were shared,
    /// for a tunnel and host with fixed ids. Every tester's autostart
    /// tunnels are in such a file, so a change in its shape, or in what
    /// the fingerprint covers, would stop them all starting.
    const OLD_FILE: &str = "[tunnels]\n\
        6f1c2d3e-0000-4000-8000-000000000002 = \
        \"b7c88b8c37bffb92d1535a59e206354a3903646d47c2e2ed0e30da596dbc1bd0\"\n";

    #[test]
    fn reads_and_writes_consents_as_burrow_always_has() {
        let host = Node {
            id: NodeId("6f1c2d3e-0000-4000-8000-000000000001".parse().unwrap()),
            kind: SSH_HOST_KIND.into(),
            name: "web".into(),
            ..node("alias = \"web\"")
        };
        let tunnel = Node {
            id: NodeId("6f1c2d3e-0000-4000-8000-000000000002".parse().unwrap()),
            ..node(&format!(
                "via = \"{}\"\ntype = \"local\"\nport = 5433\n\
                 target_host = \"db\"\ntarget_port = 5432",
                host.id
            ))
        };
        let nodes = [host, tunnel.clone()];
        let lookup = |id| nodes.iter().find(|n| n.id == id).cloned();
        let temp = tempfile::TempDir::new().unwrap();
        let path = temp.path().join("autostart.toml");
        std::fs::write(&path, OLD_FILE).unwrap();
        assert_eq!(Autostart::load(path.clone()).enabled(&lookup), [tunnel.id]);

        std::fs::remove_file(&path).unwrap();
        Autostart::load(path.clone())
            .set(&lookup, &tunnel, true)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), OLD_FILE);
    }
}
