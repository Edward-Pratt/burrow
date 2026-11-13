//! The MCP bridge's audit log: what each line keeps of a command, its
//! long strings and its error.

use data_encoding::HEXLOWER;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::api::Command;

/// The longest string the audit log keeps whole, so a few lines can't
/// fill the log and push out what came before. See [`cut`].
pub(super) const AUDIT_FIELD: usize = 4 << 10;

/// What the MCP bridge may do with a command.
pub(super) enum Rule {
    Run,
    /// A query that must not write, whatever the session allows.
    ReadOnlyQuery,
    /// A monitor that runs only the agent the user put on the server.
    MonitorWithoutInstall,
    /// Only the alerts of hosts it may monitor.
    GrantedAlerts,
}

impl Rule {
    /// How the audit log names it.
    pub(super) fn name(&self) -> &'static str {
        match self {
            Rule::Run => "run",
            Rule::ReadOnlyQuery => "read_only_query",
            Rule::MonitorWithoutInstall => "monitor_without_install",
            Rule::GrantedAlerts => "granted_alerts",
        }
    }
}

/// What the audit log keeps of a command, and the `rule` it runs under
/// when it may run at all. A refused one is kept by type only, since it
/// may carry a secret; file contents are kept by size and hash.
pub(super) fn audit_entry(command: &Command, rule: Option<&Rule>, connection: u64) -> Value {
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let mut logged = match rule {
        Some(_) => serde_json::to_value(command).unwrap_or_default(),
        None => json!({"type": command_type(command)}),
    };
    if let Command::WriteFile { data, .. } = command {
        logged["data"] = json!({"bytes": data.len(), "sha256": sha256(data)});
    }
    let mut entry = json!({"time": time, "connection": connection, "command": logged});
    if let Some(rule) = rule {
        entry["rule"] = json!(rule.name());
    }
    entry
}

pub(super) fn command_type(command: &Command) -> String {
    serde_json::to_value(command)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_string))
        .unwrap_or_default()
}

/// Replaces every string in a command that is longer than
/// [`AUDIT_FIELD`] with what [`cut`] keeps of it.
pub(super) fn cap_strings(value: &mut Value) {
    match value {
        Value::String(text) if text.len() > AUDIT_FIELD => {
            let kept = cut(text);
            *value = kept;
        }
        Value::Array(items) => items.iter_mut().for_each(cap_strings),
        Value::Object(fields) => fields.values_mut().for_each(cap_strings),
        _ => {}
    }
}

/// What the audit log keeps of a long string in a command: its start and
/// its end, since a harmless start can hide what a command really does,
/// and the whole value's length and SHA-256, so what was left out can be
/// checked against a copy of the command.
fn cut(text: &str) -> Value {
    let half = AUDIT_FIELD / 2;
    let mut head = half;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - half;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    json!({
        "head": &text[..head],
        "tail": &text[tail..],
        "bytes": text.len(),
        "sha256": sha256(text.as_bytes()),
    })
}

/// An error's start, and how long it was. An error says why a command
/// failed rather than what it did, so its start is enough.
pub(super) fn cut_error(text: &str) -> String {
    if text.len() <= AUDIT_FIELD {
        return text.to_string();
    }
    let mut end = AUDIT_FIELD;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &text[..end], text.len())
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    HEXLOWER.encode(&Sha256::digest(bytes))
}
