//! Routes to a network service, shared by every connector that speaks to
//! one. A route ends at a TCP port or a Unix socket, either on this
//! computer or as seen from an SSH host. Through an SSH host it is a
//! channel on that host's shared connection, so nothing listens locally
//! and the user signs in to the host once.
//!
//! A route only carries bytes. Whatever runs over it (a database protocol,
//! its TLS) is up to the connector, which can dial the same route again
//! when it needs a second connection, such as to cancel a query.

pub(crate) mod tls;

use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, UnixStream};

use crate::api::{Node, NodeId, SSH_HOST_KIND};
use crate::engine::Core;
use crate::ssh::{Connection, Prompter};

/// How long a direct TCP connection gets to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The longest Unix socket path Linux can connect to (`sun_path` less its
/// terminating NUL).
const MAX_SOCKET_PATH: usize = 107;

type Result<T> = std::result::Result<T, String>;

/// Where a route ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Tcp { host: String, port: u16 },
    Unix(PathBuf),
}

impl Target {
    /// A TCP target, checked for shape only: through an SSH host the name is
    /// resolved there.
    pub fn tcp(field: &str, host: &str, port: u16) -> Result<Target> {
        check_host(field, host)?;
        Ok(Target::Tcp {
            host: host.to_string(),
            port,
        })
    }

    /// A Unix socket target. Connectors build the path themselves from
    /// checked parts, so a node file can't aim a connector at an arbitrary
    /// socket such as the SSH agent's.
    pub fn unix(path: PathBuf) -> Result<Target> {
        check_path("socket", &path)?;
        if path.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(format!(
                "socket path {} is longer than {MAX_SOCKET_PATH} bytes",
                path.display()
            ));
        }
        Ok(Target::Unix(path))
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Tcp { host, port } if host.contains(':') => write!(f, "[{host}]:{port}"),
            Target::Tcp { host, port } => write!(f, "{host}:{port}"),
            Target::Unix(path) => write!(f, "{}", path.display()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    /// The SSH host node it goes through, if any.
    pub via: Option<NodeId>,
    pub target: Target,
}

/// Any byte stream a route can produce.
trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// An open route. Through an SSH host it holds on to the connection, which
/// would otherwise close when its last session lets go.
pub(crate) struct Link {
    io: Box<dyn Io>,
    _ssh: Option<Arc<Connection>>,
}

/// The SSH host node a route goes through, which must exist and be an SSH
/// host.
pub(crate) fn ssh_host(core: &Core, via: NodeId) -> Result<Node> {
    let host = core
        .node(via)
        .map_err(|_| "the SSH host it goes through no longer exists".to_string())?;
    if host.kind != SSH_HOST_KIND {
        return Err(format!("{} is a {}, not an SSH host", host.name, host.kind));
    }
    Ok(host)
}

/// Opens `route`. Through an SSH host, questions asked while connecting
/// (host key, passwords) go to `prompter`'s session, and `step` reports
/// progress.
pub(crate) async fn dial(
    core: &Core,
    route: &Route,
    prompter: &Prompter,
    step: &impl Fn(String),
) -> Result<Link> {
    let Some(via) = route.via else {
        step(format!("Connecting to {}", route.target));
        return dial_direct(&route.target).await;
    };

    let host = ssh_host(core, via)?;
    let connection = core.connections().get(core, &host, prompter, step).await?;
    step(format!("Opening {} through {}", route.target, host.name));
    let handle = connection.handle();
    let opened = match &route.target {
        Target::Tcp { host, port } => {
            handle
                .channel_open_direct_tcpip(host.as_str(), (*port).into(), "127.0.0.1", 0)
                .await
        }
        Target::Unix(path) => {
            handle
                .channel_open_direct_streamlocal(path.to_string_lossy())
                .await
        }
    };
    let channel =
        opened.map_err(|e| format!("{} could not open {}: {e}", host.name, route.target))?;
    Ok(Link {
        io: Box::new(channel.into_stream()),
        _ssh: Some(connection),
    })
}

/// Opens a target on this computer.
async fn dial_direct(target: &Target) -> Result<Link> {
    let io: Box<dyn Io> = match target {
        Target::Tcp { host, port } => {
            let connect = TcpStream::connect((host.as_str(), *port));
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, connect)
                .await
                .map_err(|_| format!("{target} did not answer"))?
                .map_err(|e| format!("could not connect to {target}: {e}"))?;
            let _ = stream.set_nodelay(true);
            Box::new(stream)
        }
        Target::Unix(path) => Box::new(
            UnixStream::connect(path)
                .await
                .map_err(|e| format!("could not connect to {target}: {e}"))?,
        ),
    };
    Ok(Link { io, _ssh: None })
}

impl AsyncRead for Link {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for Link {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut *self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.io).poll_shutdown(cx)
    }
}

/// A host name or address, checked for shape only, since it may be
/// resolved on an SSH host.
pub(crate) fn check_host(field: &str, host: &str) -> Result<()> {
    let ok = !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host.chars().all(|c| c.is_ascii_graphic() && c != '/');
    if ok {
        Ok(())
    } else {
        Err(format!("{field} {host:?} is not a host name or address"))
    }
}

/// An absolute path with no `..` and no control characters.
pub(crate) fn check_path(field: &str, path: &Path) -> Result<()> {
    let text = path.to_string_lossy();
    if !path.is_absolute() {
        return Err(format!("{field} {text:?} is not an absolute path"));
    }
    if path.components().any(|c| c == Component::ParentDir) {
        return Err(format!("{field} {text:?} may not contain \"..\""));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{field} {text:?} holds control characters"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names_are_checked_for_shape() {
        for good in ["db.internal", "10.0.0.5", "::1", "localhost"] {
            assert!(check_host("host", good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "-oProxyCommand=x",
            "a b",
            "a/b",
            "db\n",
            &"a".repeat(256),
        ] {
            assert!(check_host("host", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn socket_paths_must_be_absolute_short_and_plain() {
        assert!(Target::unix("/run/postgresql/.s.PGSQL.5432".into()).is_ok());
        for bad in [
            "run/postgresql/.s.PGSQL.5432",
            "/run/../home/me/.ssh/agent.sock",
            "/run/post\ngresql/.s.PGSQL.5432",
        ] {
            assert!(Target::unix(bad.into()).is_err(), "{bad:?}");
        }
        let long = format!("/{}", "a".repeat(MAX_SOCKET_PATH));
        assert!(Target::unix(long.into()).is_err());
    }

    #[test]
    fn targets_read_as_addresses() {
        let tcp = |host: &str| Target::tcp("host", host, 5432).unwrap().to_string();
        assert_eq!(tcp("db.internal"), "db.internal:5432");
        assert_eq!(tcp("::1"), "[::1]:5432");
        let unix = Target::unix("/run/postgresql/.s.PGSQL.5432".into()).unwrap();
        assert_eq!(unix.to_string(), "/run/postgresql/.s.PGSQL.5432");
    }

    #[test]
    fn unix_sockets_can_be_dialed_directly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("s");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                socket.write_all(b"hi").await.unwrap();
            });
            let target = Target::unix(path.clone()).unwrap();
            let mut link = dial_direct(&target).await.unwrap();
            let mut got = [0; 2];
            link.read_exact(&mut got).await.unwrap();
            assert_eq!(&got, b"hi");
        });
    }
}
