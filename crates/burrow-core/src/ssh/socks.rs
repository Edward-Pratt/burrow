//! The server side of SOCKS5 (RFC 1928), just enough for `ssh -D`: no
//! authentication and the CONNECT command. The tunnel then opens the
//! destination through the SSH host and answers with [`reply`].

use std::io::{Error, ErrorKind, Result};
use std::net::{Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const VERSION: u8 = 5;
const NO_AUTHENTICATION: u8 = 0;
const NO_ACCEPTABLE_METHOD: u8 = 0xff;
const CONNECT: u8 = 1;

pub const SUCCEEDED: u8 = 0;
pub const REFUSED: u8 = 5;
const COMMAND_NOT_SUPPORTED: u8 = 7;
const ADDRESS_NOT_SUPPORTED: u8 = 8;

/// Reads a client's greeting and CONNECT request, and returns where it
/// wants to go. The client still waits for [`reply`].
pub async fn accept(socket: &mut TcpStream) -> Result<(String, u16)> {
    let [version, count] = read::<2>(socket).await?;
    if version != VERSION {
        return Err(invalid(format!("SOCKS version {version} is not supported")));
    }
    let mut methods = vec![0; count.into()];
    socket.read_exact(&mut methods).await?;
    if !methods.contains(&NO_AUTHENTICATION) {
        socket.write_all(&[VERSION, NO_ACCEPTABLE_METHOD]).await?;
        return Err(invalid("the client wants authentication".into()));
    }
    socket.write_all(&[VERSION, NO_AUTHENTICATION]).await?;

    let [version, command, _, address_type] = read::<4>(socket).await?;
    if version != VERSION {
        return Err(invalid(format!("SOCKS version {version} is not supported")));
    }
    let host = match address_type {
        1 => Ipv4Addr::from(read::<4>(socket).await?).to_string(),
        3 => {
            let [length] = read::<1>(socket).await?;
            let mut name = vec![0; length.into()];
            socket.read_exact(&mut name).await?;
            match String::from_utf8(name) {
                Ok(name) if !name.is_empty() => name,
                _ => {
                    reply(socket, ADDRESS_NOT_SUPPORTED).await?;
                    return Err(invalid("the host name is not valid".into()));
                }
            }
        }
        4 => Ipv6Addr::from(read::<16>(socket).await?).to_string(),
        other => {
            reply(socket, ADDRESS_NOT_SUPPORTED).await?;
            return Err(invalid(format!("address type {other} is not supported")));
        }
    };
    let port = u16::from_be_bytes(read::<2>(socket).await?);
    if command != CONNECT {
        reply(socket, COMMAND_NOT_SUPPORTED).await?;
        return Err(invalid(format!("SOCKS command {command} is not supported")));
    }
    Ok((host, port))
}

/// Answers a CONNECT request. The bound address is left as 0.0.0.0:0, which
/// clients ignore.
pub async fn reply(socket: &mut TcpStream, code: u8) -> Result<()> {
    socket
        .write_all(&[VERSION, code, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
}

async fn read<const N: usize>(socket: &mut TcpStream) -> Result<[u8; N]> {
    let mut buffer = [0; N];
    socket.read_exact(&mut buffer).await?;
    Ok(buffer)
}

fn invalid(message: String) -> Error {
    Error::new(ErrorKind::InvalidData, message)
}
