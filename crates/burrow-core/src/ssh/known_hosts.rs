//! Host key checks against OpenSSH `known_hosts` files.
//!
//! Burrow reads and writes the same files as `ssh`, so a host trusted in
//! one is trusted in the other. russh has helpers for this, but they stop at
//! the first line they can't parse and ignore `@revoked`, wildcards and
//! negated patterns, which real files contain.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::PublicKey;
use sha1::Sha1;

/// What the `known_hosts` files say about a host's key.
#[derive(Debug, PartialEq, Eq)]
pub enum Check {
    Known,
    /// No line for this host has this key or another key of its type.
    Unknown,
    /// A line for this host has a different key of the same type: the host
    /// was reinstalled, or someone is in the middle.
    Changed {
        path: PathBuf,
        line: usize,
    },
    /// The key is marked `@revoked`.
    Revoked {
        path: PathBuf,
        line: usize,
    },
}

/// Checks `key` for `host` (the `HostName` or `HostKeyAlias`) on `port`.
/// Missing files count as empty; unreadable lines are skipped.
pub fn check(files: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> Check {
    let name = lookup_name(host, port);
    let mut known = false;
    let mut changed = None;
    for path in files {
        let Ok(file) = File::open(path) else { continue };
        for (index, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else { break };
            let Some(entry) = Entry::parse(&line) else {
                continue;
            };
            if !entry.matches(&name) {
                continue;
            }
            let at = || (path.clone(), index + 1);
            let same = entry.key.key_data() == key.key_data();
            match entry.marker {
                Some(Marker::Revoked) if same => {
                    let (path, line) = at();
                    return Check::Revoked { path, line };
                }
                Some(_) => {}
                None if same => known = true,
                None if entry.key.algorithm() == key.algorithm() => {
                    changed.get_or_insert_with(at);
                }
                None => {}
            }
        }
    }
    match (known, changed) {
        (true, _) => Check::Known,
        (false, Some((path, line))) => Check::Changed { path, line },
        (false, None) => Check::Unknown,
    }
}

/// Appends `key` for `host` to `path`, hashing the host name when `hashed`
/// (OpenSSH's `HashKnownHosts`).
pub fn learn(
    path: &Path,
    host: &str,
    port: u16,
    key: &PublicKey,
    hashed: bool,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    // Start on a new line if the file doesn't end with one.
    let mut last = [0u8];
    let needs_newline = file.seek(SeekFrom::End(-1)).is_ok()
        && file.read_exact(&mut last).is_ok()
        && last != *b"\n";

    let name = lookup_name(host, port);
    let pattern = if hashed { hash(&name)? } else { name };
    let key = key.to_openssh().map_err(std::io::Error::other)?;
    let line = format!("{}{pattern} {key}\n", if needs_newline { "\n" } else { "" });
    file.write_all(line.as_bytes())
}

/// `host`, or `[host]:port` off port 22, as OpenSSH records it.
fn lookup_name(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

#[derive(Debug, PartialEq)]
enum Marker {
    Revoked,
    CertAuthority,
}

struct Entry {
    marker: Option<Marker>,
    patterns: String,
    key: PublicKey,
}

impl Entry {
    /// `[@marker] patterns keytype base64 [comment]`.
    fn parse(line: &str) -> Option<Entry> {
        let mut fields = line.split_whitespace();
        let mut first = fields.next()?;
        if first.starts_with('#') {
            return None;
        }
        let marker = match first {
            "@revoked" => Some(Marker::Revoked),
            "@cert-authority" => Some(Marker::CertAuthority),
            _ => None,
        };
        if marker.is_some() {
            first = fields.next()?;
        }
        let (kind, data) = (fields.next()?, fields.next()?);
        let key = PublicKey::from_openssh(&format!("{kind} {data}")).ok()?;
        Some(Entry {
            marker,
            patterns: first.to_string(),
            key,
        })
    }

    /// A negated pattern that matches rules the line out, whatever else
    /// matches.
    fn matches(&self, name: &str) -> bool {
        let mut matched = false;
        for pattern in self.patterns.split(',') {
            if let Some(negated) = pattern.strip_prefix('!') {
                if wildcard(negated, name) {
                    return false;
                }
            } else if pattern.starts_with("|1|") {
                matched |= hashed_matches(pattern, name);
            } else {
                matched |= wildcard(pattern, name);
            }
        }
        matched
    }
}

/// `|1|base64(salt)|base64(HMAC-SHA1(salt, name))`.
fn hashed_matches(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('|').skip(2);
    let (Some(salt), Some(hash)) = (parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(name.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

fn hash(name: &str) -> std::io::Result<String> {
    let mut salt = [0u8; 20];
    getrandom::fill(&mut salt).map_err(std::io::Error::other)?;
    let mac = Hmac::<Sha1>::new_from_slice(&salt)
        .map_err(std::io::Error::other)?
        .chain_update(name.as_bytes())
        .finalize()
        .into_bytes();
    Ok(format!(
        "|1|{}|{}",
        BASE64.encode(&salt),
        BASE64.encode(&mac)
    ))
}

/// OpenSSH patterns: `*` is any run of characters, `?` one character, and
/// letters match regardless of case.
fn wildcard(pattern: &str, name: &str) -> bool {
    fn go(p: &[u8], n: &[u8]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((b'*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some((b'?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n
                .split_first()
                .is_some_and(|(d, n)| c.eq_ignore_ascii_case(d) && go(rest, n)),
        }
    }
    go(pattern.as_bytes(), name.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ED_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICbaiBapizz/rS2dy01Wmdl4qosnKAUAO5rH+5ql969t";
    const ED_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKZGVcXH1TCdybLGEz1fjmEN1yCxJgY1Jv2gMRBFFdxO";
    const ECDSA: &str = "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBJ3U/dmNDCPyI4rqVQdncaSUbuVRa7XoTIQ5tA4YAXyUc2kbjkwXg3LmIXHupvoSjlwQktHQ1UeK7seZ1UzxhXI=";

    fn key(text: &str) -> PublicKey {
        PublicKey::from_openssh(text).unwrap()
    }

    fn file(dir: &tempfile::TempDir, text: &str) -> PathBuf {
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn plain_entries() {
        let dir = tempfile::tempdir().unwrap();
        let files = [file(
            &dir,
            &format!("# comment\nexample.com,10.0.0.1 {ED_A}\n"),
        )];
        assert_eq!(check(&files, "example.com", 22, &key(ED_A)), Check::Known);
        assert_eq!(check(&files, "10.0.0.1", 22, &key(ED_A)), Check::Known);
        assert_eq!(check(&files, "other.com", 22, &key(ED_A)), Check::Unknown);
        // The same host on another port is another entry.
        assert_eq!(
            check(&files, "example.com", 2222, &key(ED_A)),
            Check::Unknown
        );
    }

    #[test]
    fn a_different_key_of_the_same_type_is_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, &format!("\nexample.com {ED_A}\n"));
        let files = [path.clone()];
        assert_eq!(
            check(&files, "example.com", 22, &key(ED_B)),
            Check::Changed { path, line: 2 }
        );
        // A key of another type is just not known yet.
        assert_eq!(
            check(&files, "example.com", 22, &key(ECDSA)),
            Check::Unknown
        );
    }

    #[test]
    fn revoked_keys_are_refused_even_when_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, &format!("example.com {ED_A}\n@revoked * {ED_A}\n"));
        assert_eq!(
            check(std::slice::from_ref(&path), "example.com", 22, &key(ED_A)),
            Check::Revoked { path, line: 2 }
        );
    }

    #[test]
    fn wildcards_and_negation() {
        let dir = tempfile::tempdir().unwrap();
        let files = [file(
            &dir,
            &format!("*.example.com,!bad.example.com {ED_A}\n"),
        )];
        assert_eq!(
            check(&files, "web.EXAMPLE.com", 22, &key(ED_A)),
            Check::Known
        );
        assert_eq!(
            check(&files, "bad.example.com", 22, &key(ED_A)),
            Check::Unknown
        );
        assert!(wildcard("web-?", "web-1"));
        assert!(!wildcard("web-?", "web-10"));
    }

    #[test]
    fn bad_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let text = format!(
            "garbage\nhost ssh-mldsa44 AAAAnotakey\n@cert-authority *.corp {ED_B}\n\
             example.com\t{ED_A} comment here\n"
        );
        let files = [file(&dir, &text)];
        assert_eq!(check(&files, "example.com", 22, &key(ED_A)), Check::Known);
    }

    #[test]
    fn learned_keys_are_found_plain_and_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, &format!("other {ECDSA}"));
        learn(&path, "example.com", 2222, &key(ED_A), false).unwrap();
        learn(&path, "hidden.com", 22, &key(ED_B), true).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\n[example.com]:2222 ssh-ed25519 "));
        assert!(!text.contains("hidden.com"));
        let files = [path];
        assert_eq!(check(&files, "example.com", 2222, &key(ED_A)), Check::Known);
        assert_eq!(check(&files, "hidden.com", 22, &key(ED_B)), Check::Known);
        assert_eq!(check(&files, "other", 22, &key(ECDSA)), Check::Known);
    }

    #[test]
    fn missing_files_are_empty() {
        let files = [PathBuf::from("/nonexistent/known_hosts")];
        assert_eq!(check(&files, "example.com", 22, &key(ED_A)), Check::Unknown);
    }
}
