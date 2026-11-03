//! The command/event API. These types are the contract between the core and
//! every client, so changes here must bump [`API_VERSION`] when they are not
//! backwards compatible. All of them serialize to JSON for out-of-process
//! clients (addons, CLI, MCP bridge).
//!
//! The types are grouped by what they describe: `nodes`, `sessions`,
//! `addons`, `grants`, `hosts` and `db`, then `command`, `response` and
//! `event`. All of them are reached here, as `crate::api::Name`.

mod addons;
mod command;
mod db;
mod event;
mod grants;
mod hosts;
mod nodes;
mod response;
mod sessions;

pub use addons::*;
pub use command::*;
pub use db::*;
pub use event::*;
pub use grants::*;
pub use hosts::*;
pub use nodes::*;
pub use response::*;
pub use sessions::*;

/// What burrow-agent reports, as monitor events carry it.
pub use burrow_agent as monitor;

/// Major version of the command/event API.
pub const API_VERSION: u32 = 1;

/// The `kind` of folder nodes. Every other kind is owned by an addon.
pub const FOLDER_KIND: &str = "folder";

/// The `kind` of SSH host nodes. `props.alias` names the `Host` in the
/// user's OpenSSH config; the node's name is used when it is missing.
pub const SSH_HOST_KIND: &str = "ssh-host";

/// The `kind` of SSH tunnel nodes. Their props say which host to go
/// through and what to forward; see [`Command::StartTunnel`].
pub const SSH_TUNNEL_KIND: &str = "ssh-tunnel";

/// The `kind` of VNC desktop nodes, reached through an SSH host; see
/// [`Command::OpenVnc`].
pub const VNC_KIND: &str = "vnc";

/// The `kind` of PostgreSQL connection nodes. Their props say where the
/// server is and how to reach it; see [`Command::OpenDatabase`].
pub const POSTGRES_KIND: &str = "postgres";

/// The SHA-256 of `data` in lower-case hex, as [`Response::File`] gives
/// it and [`Command::WriteFile`]'s `expect` takes it.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

/// Bytes as a base64 string in JSON, rather than an array of numbers.
mod base64_bytes {
    use data_encoding::BASE64;
    use serde::de::{self, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&BASE64.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        BASE64
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }

    /// Base64, or an array of numbers as clients sent for
    /// [`Command::WriteTerminal`](super::Command::WriteTerminal) before
    /// it took base64, so those clients still work.
    pub fn deserialize_either<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        struct Bytes;
        impl<'de> Visitor<'de> for Bytes {
            type Value = Vec<u8>;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("base64 or an array of bytes")
            }

            fn visit_str<E: de::Error>(self, text: &str) -> Result<Vec<u8>, E> {
                BASE64.decode(text.as_bytes()).map_err(E::custom)
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut bytes = Vec::new();
                while let Some(byte) = seq.next_element()? {
                    bytes.push(byte);
                }
                Ok(bytes)
            }
        }
        deserializer.deserialize_any(Bytes)
    }
}
