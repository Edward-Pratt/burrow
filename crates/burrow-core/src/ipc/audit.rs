//! What the MCP bridge's audit log keeps of long strings and errors.

use data_encoding::HEXLOWER;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The longest string the audit log keeps whole, so a few lines can't
/// fill the log and push out what came before. See [`cut`].
pub(super) const AUDIT_FIELD: usize = 4 << 10;

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
