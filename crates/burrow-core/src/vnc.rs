//! VNC desktops, reached through an SSH host. The core signs in to the VNC
//! server, so the password stays here. It then plays a server that needs
//! no password to the client, and passes bytes both ways: the client
//! speaks RFB from there on and draws the screen.

use std::time::Duration;

use des::Des;
use des::cipher::{BlockCipherEncrypt, KeyInit};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::api::{
    Event, Node, NodeId, PromptAnswer, PromptKind, SessionId, SessionKind, SessionState,
};
use crate::engine::Core;
use crate::net::{self, Route, Target};
use crate::password::{Saved, save_password, saved_password};
use crate::ssh::{self, Input, Prompter};

type Result<T> = std::result::Result<T, String>;

const DEFAULT_PORT: u16 = 5900;

/// How long the server gets to finish signing in.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a wrong password is asked for again. VNC servers stop
/// answering a client that fails too often.
const ATTEMPTS: usize = 3;

/// The longest reason a server may give for refusing.
const MAX_REASON: usize = 4096;

/// What the client is told: RFB 3.8, the version it asks for too.
const VERSION: &[u8; 12] = b"RFB 003.008\n";

/// RFB security types.
const SECURITY_NONE: u8 = 1;
const SECURITY_VNC: u8 = 2;

/// Where a `vnc` node's desktop is.
#[derive(Debug, PartialEq)]
pub(crate) struct Spec {
    route: Route,
}

impl Spec {
    /// From a node's props: `via`, the SSH host it goes through, and the
    /// server's `host` and `port` as that host sees them.
    pub fn from_node(node: &Node) -> Result<Spec> {
        let props = &node.props;
        let via = props
            .get("via")
            .and_then(|via| via.as_str())
            .ok_or("via is missing: a desktop is reached through an SSH host")?;
        let via = via
            .parse()
            .map(NodeId)
            .map_err(|_| "via must be the id of an SSH host".to_string())?;
        let host = match props.get("host") {
            None => "localhost",
            Some(host) => host.as_str().ok_or("host must be text")?,
        };
        let port = match props.get("port") {
            None => DEFAULT_PORT,
            Some(value) => value
                .as_integer()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
                .ok_or("port must be a port number from 1 to 65535")?,
        };
        Ok(Spec {
            route: Route {
                via: Some(via),
                target: Target::tcp("host", host, port)?,
            },
        })
    }

    /// What a saved password is for, so it is only sent there.
    fn password_target(&self, host: &Node) -> String {
        let alias = crate::ssh::alias(host);
        format!(
            "vnc {:?} through {alias:?} ({})",
            self.route.target.to_string(),
            host.id
        )
    }
}

/// Starts a session showing a `vnc` node's desktop. Its output is the
/// server's RFB stream after the stand-in handshake.
pub(crate) fn open(core: &Core, node: Node) -> SessionId {
    let (output_tx, output_rx) = mpsc::channel(ssh::OUTPUT_BUFFER);
    ssh::spawn(
        core,
        node,
        SessionKind::Vnc,
        Some(output_rx),
        move |core, node, input, prompter| async move {
            run(&core, &node, input, output_tx, &prompter).await
        },
    )
}

async fn run(
    core: &Core,
    node: &Node,
    mut input: mpsc::UnboundedReceiver<Input>,
    output: mpsc::Sender<Vec<u8>>,
    prompter: &Prompter,
) -> Result<()> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session(),
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let spec = Spec::from_node(node)?;
    let host = net::ssh_host(core, spec.route.via.expect("a desktop has via"))?;
    let target = spec.password_target(&host);
    let saved = saved_password(core, node, &target).await;
    let mut password = match &saved {
        Saved::Matches(password) => Some(password.clone()),
        _ => None,
    };
    let (mut save, mut asked) = (false, 0);
    let mut link = loop {
        let mut link = net::dial(core, &spec.route, prompter, &step).await?;
        step("Signing in to the desktop".into());
        let signed_in = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            sign_in(&mut link, password.as_ref().map(|p| p.expose())),
        )
        .await
        .unwrap_or_else(|_| Err(SignIn::Failed("the desktop didn't answer".into())));
        let refused = match signed_in {
            Ok(()) => break link,
            Err(SignIn::Failed(error)) => return Err(error),
            Err(SignIn::NeedsPassword) => None,
            Err(SignIn::Refused(reason)) => Some(reason),
        };
        if asked == ATTEMPTS {
            return Err(refused.unwrap_or_else(|| "the password was refused".into()));
        }
        let message = match (&refused, &saved) {
            (Some(reason), _) => format!("{reason}. Password for the desktop on {}", host.name),
            (None, Saved::ForElsewhere(other)) => format!(
                "The saved password is for {other}, so Burrow didn't send it. \
                 Password for the desktop on {}",
                host.name
            ),
            (None, _) => format!("Password for the desktop on {}", host.name),
        };
        asked += 1;
        match prompter
            .ask(PromptKind::Secret { can_save: true }, "Password", &message)
            .await
        {
            PromptAnswer::Text { value, save: keep } => {
                password = Some(value);
                save = keep;
            }
            _ => return Err("signing in was cancelled".into()),
        }
    };
    if save && let Some(password) = password {
        save_password(core, node, password, target).await;
    }
    core.emit(Event::SessionChanged {
        session: prompter.session(),
        node: node.id,
        state: SessionState::Connected,
    });

    let mut client = ClientSide::default();
    if !client.greet(&mut input, &output).await? {
        return Ok(());
    }
    // The client's first message, ClientInit, may have come with the
    // handshake's last byte.
    if !client.pending.is_empty() {
        link.write_all(&client.pending).await.map_err(text)?;
    }
    let mut buffer = vec![0; 64 << 10];
    loop {
        tokio::select! {
            read = link.read(&mut buffer) => match read {
                Ok(0) => return Err("the desktop closed the connection".into()),
                Ok(n) => {
                    // The client stopped reading: nobody is watching.
                    if output.send(buffer[..n].to_vec()).await.is_err() {
                        return Ok(());
                    }
                }
                Err(e) => return Err(text(e)),
            },
            message = input.recv() => match message {
                Some(Input::Write(data)) => link.write_all(&data).await.map_err(text)?,
                Some(Input::Resize { .. }) => {}
                Some(Input::Close) | None => return Ok(()),
            },
        }
    }
}

enum SignIn {
    /// The server wants a password, and none was given.
    NeedsPassword,
    /// The server refused the password, saying why.
    Refused(String),
    /// Anything else, which another try won't change.
    Failed(String),
}

impl From<std::io::Error> for SignIn {
    fn from(error: std::io::Error) -> SignIn {
        SignIn::Failed(format!("the desktop's handshake broke off: {error}"))
    }
}

/// Signs in to a VNC server, up to the point where the client's own
/// ClientInit comes next.
async fn sign_in<S>(stream: &mut S, password: Option<&str>) -> std::result::Result<(), SignIn>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut version = [0; 12];
    stream.read_exact(&mut version).await?;
    let minor = parse_version(&version).ok_or_else(|| {
        SignIn::Failed(format!(
            "this isn't a VNC server: it said {:?}",
            String::from_utf8_lossy(&version)
        ))
    })?;
    // 3.3, 3.7 and 3.8 are the versions there are. Later ones (Apple's
    // 3.889) speak 3.8 too.
    let minor = match minor {
        0..=6 => 3,
        7 => 7,
        _ => 8,
    };
    stream
        .write_all(format!("RFB 003.00{minor}\n").as_bytes())
        .await?;

    let security = if minor == 3 {
        match stream.read_u32().await? {
            0 => return Err(SignIn::Failed(reason(stream).await?)),
            kind => u8::try_from(kind).unwrap_or(0),
        }
    } else {
        let count = stream.read_u8().await?;
        if count == 0 {
            return Err(SignIn::Failed(reason(stream).await?));
        }
        let mut offered = vec![0; count.into()];
        stream.read_exact(&mut offered).await?;
        let chosen = [SECURITY_NONE, SECURITY_VNC]
            .into_iter()
            .find(|kind| offered.contains(kind))
            .ok_or_else(|| SignIn::Failed(unsupported(&offered)))?;
        stream.write_all(&[chosen]).await?;
        chosen
    };
    match security {
        SECURITY_NONE => {
            // Only 3.8 says how it went when there was nothing to check.
            if minor == 8 {
                result(stream, minor).await?;
            }
            Ok(())
        }
        SECURITY_VNC => {
            let Some(password) = password else {
                return Err(SignIn::NeedsPassword);
            };
            let mut challenge = [0; 16];
            stream.read_exact(&mut challenge).await?;
            stream.write_all(&respond(password, &challenge)).await?;
            result(stream, minor).await
        }
        other => Err(SignIn::Failed(unsupported(&[other]))),
    }
}

/// The minor version in `RFB 003.00x\n`, if it is that.
fn parse_version(line: &[u8; 12]) -> Option<u32> {
    let line = std::str::from_utf8(line).ok()?;
    let digits = line.strip_prefix("RFB 003.")?.strip_suffix('\n')?;
    (digits.len() == 3 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())?
}

/// The SecurityResult after signing in.
async fn result<S: AsyncRead + Unpin>(
    stream: &mut S,
    minor: u32,
) -> std::result::Result<(), SignIn> {
    match stream.read_u32().await? {
        0 => Ok(()),
        _ if minor == 8 => Err(SignIn::Refused(reason(stream).await?)),
        _ => Err(SignIn::Refused("the password was refused".into())),
    }
}

/// A reason string, cut down to what is worth showing.
async fn reason<S: AsyncRead + Unpin>(stream: &mut S) -> std::io::Result<String> {
    let length = stream.read_u32().await? as usize;
    let mut bytes = vec![0; length.min(MAX_REASON)];
    stream.read_exact(&mut bytes).await?;
    let text: String = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    Ok(text.trim_end_matches('.').to_string())
}

fn unsupported(offered: &[u8]) -> String {
    let names: Vec<String> = offered
        .iter()
        .map(|kind| match kind {
            5 => "RA2".into(),
            6 => "RA2ne".into(),
            16 => "Tight".into(),
            18 => "TLS".into(),
            19 => "VeNCrypt".into(),
            30 => "Apple".into(),
            kind => format!("type {kind}"),
        })
        .collect();
    format!(
        "the desktop wants {} to sign in, and Burrow only knows a VNC password or none",
        names.join(", ")
    )
}

/// VNC authentication: the challenge encrypted with DES, keyed by the
/// password's first 8 bytes with each byte's bits in reverse order.
fn respond(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let cipher = Des::new_from_slice(&key).expect("a DES key is 8 bytes");
    let mut response = *challenge;
    for half in response.as_chunks_mut::<8>().0 {
        cipher.encrypt_block(half.into());
    }
    response
}

/// The client's end of the stand-in handshake: version 3.8, and no
/// password, since the core has already signed in.
#[derive(Default)]
struct ClientSide {
    /// Bytes the client sent past what the handshake needed.
    pending: Vec<u8>,
}

impl ClientSide {
    /// Plays the server's part. `false` if the client closed meanwhile.
    async fn greet(
        &mut self,
        input: &mut mpsc::UnboundedReceiver<Input>,
        output: &mpsc::Sender<Vec<u8>>,
    ) -> Result<bool> {
        let send = |data: Vec<u8>| async move { output.send(data).await.is_ok() };
        if !send(VERSION.to_vec()).await {
            return Ok(false);
        }
        let Some(version) = self.take(input, 12).await else {
            return Ok(false);
        };
        if version != VERSION {
            return Err(format!(
                "the window asked for {:?}, and Burrow speaks RFB 3.8",
                String::from_utf8_lossy(&version)
            ));
        }
        if !send(vec![1, SECURITY_NONE]).await {
            return Ok(false);
        }
        match self.take(input, 1).await {
            Some(kind) if kind == [SECURITY_NONE] => {}
            Some(_) => return Err("the window asked to sign in again".into()),
            None => return Ok(false),
        }
        Ok(send(vec![0; 4]).await)
    }

    /// The next `n` bytes the client writes, or `None` if it closes first.
    async fn take(
        &mut self,
        input: &mut mpsc::UnboundedReceiver<Input>,
        n: usize,
    ) -> Option<Vec<u8>> {
        while self.pending.len() < n {
            match input.recv().await? {
                Input::Write(data) => self.pending.extend(data),
                Input::Resize { .. } => {}
                Input::Close => return None,
            }
        }
        Some(self.pending.drain(..n).collect())
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHALLENGE: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn vnc_authentication_matches_openssl() {
        // `openssl enc -des-ecb -nopad` with the bit-reversed key.
        assert_eq!(
            hex(&respond("secret12", &CHALLENGE)),
            "adcd997f8e16fee575e973f93c2b62b4"
        );
        // Short passwords are padded with zeros, long ones cut to 8.
        assert_eq!(
            hex(&respond("pw", &CHALLENGE)),
            "858600d9af143c9e6541d3dd92a835d0"
        );
        assert_eq!(
            respond("secret12 and more", &CHALLENGE),
            respond("secret12", &CHALLENGE)
        );
    }

    #[test]
    fn versions_are_read_strictly() {
        assert_eq!(parse_version(b"RFB 003.008\n"), Some(8));
        assert_eq!(parse_version(b"RFB 003.889\n"), Some(889));
        assert_eq!(parse_version(b"RFB 004.001\n"), None);
        assert_eq!(parse_version(b"SSH-2.0-Ope\n"), None);
    }

    #[test]
    fn a_desktop_needs_an_ssh_host() {
        let node = |props: &str| Node {
            id: NodeId::new(),
            parent: None,
            kind: crate::api::VNC_KIND.into(),
            name: "desktop".into(),
            color: None,
            props: toml::from_str(props).unwrap(),
            secrets: Default::default(),
        };
        let via = "via = \"6f1c2a4e-0000-4000-8000-000000000000\"";
        let spec = Spec::from_node(&node(via)).unwrap();
        assert_eq!(spec.route.target.to_string(), "localhost:5900");
        let spec =
            Spec::from_node(&node(&format!("{via}\nhost = \"10.0.0.2\"\nport = 5901"))).unwrap();
        assert_eq!(spec.route.target.to_string(), "10.0.0.2:5901");
        assert!(Spec::from_node(&node("host = \"desktop\"")).is_err());
        assert!(Spec::from_node(&node(&format!("{via}\nport = 0"))).is_err());
        assert!(Spec::from_node(&node(&format!("{via}\nhost = \"-oProxy\""))).is_err());
    }

    /// A server that offers `offered`, takes the password "secret12", and
    /// then echoes.
    async fn server(stream: tokio::io::DuplexStream, offered: Vec<u8>) {
        let mut stream = stream;
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        assert_eq!(&version, b"RFB 003.008\n");
        stream.write_all(&[offered.len() as u8]).await.unwrap();
        stream.write_all(&offered).await.unwrap();
        let chosen = stream.read_u8().await.unwrap();
        if chosen == SECURITY_VNC {
            stream.write_all(&CHALLENGE).await.unwrap();
            let mut answer = [0; 16];
            stream.read_exact(&mut answer).await.unwrap();
            if answer != respond("secret12", &CHALLENGE) {
                stream.write_all(&1u32.to_be_bytes()).await.unwrap();
                let why = b"Authentication failed.";
                stream
                    .write_all(&(why.len() as u32).to_be_bytes())
                    .await
                    .unwrap();
                stream.write_all(why).await.unwrap();
                return;
            }
        }
        stream.write_all(&0u32.to_be_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn signing_in_asks_for_what_the_server_wants() {
        let run = |offered: Vec<u8>, password: Option<&'static str>| async move {
            let (mut ours, theirs) = tokio::io::duplex(1024);
            let server = tokio::spawn(server(theirs, offered));
            let result = sign_in(&mut ours, password).await;
            drop(ours);
            let _ = server.await;
            result
        };
        assert!(run(vec![SECURITY_NONE, SECURITY_VNC], None).await.is_ok());
        assert!(run(vec![SECURITY_VNC], Some("secret12")).await.is_ok());
        assert!(matches!(
            run(vec![SECURITY_VNC], None).await,
            Err(SignIn::NeedsPassword)
        ));
        match run(vec![SECURITY_VNC], Some("wrong")).await {
            Err(SignIn::Refused(reason)) => assert_eq!(reason, "Authentication failed"),
            _ => panic!("a wrong password should be refused"),
        }
        match run(vec![19, 18], None).await {
            Err(SignIn::Failed(error)) => assert!(error.contains("VeNCrypt, TLS"), "{error}"),
            _ => panic!("VeNCrypt alone should fail"),
        }
    }
}
