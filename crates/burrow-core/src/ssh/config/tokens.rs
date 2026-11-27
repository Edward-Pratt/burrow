//! The `%` tokens OpenSSH expands in `IdentityFile`. `ssh -G` prints them
//! as written, for the client to fill in once it knows the host, so a key
//! named `~/.ssh/key_%h` would otherwise never be found.

use std::path::PathBuf;

use sha1::{Digest, Sha1};

use super::HostConfig;

/// This computer's side of the tokens.
#[derive(Clone, Debug)]
pub(super) struct Local {
    pub home: PathBuf,
    pub user: String,
    pub uid: u32,
    /// The host name, as `gethostname` gives it.
    pub host: String,
}

impl Local {
    pub fn current() -> Self {
        let uid = crate::ipc::own_uid();
        Local {
            home: std::env::home_dir().unwrap_or_default(),
            user: user_name(uid).unwrap_or_default(),
            uid,
            host: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|h| h.trim().to_string())
                .unwrap_or_default(),
        }
    }
}

/// Expands the tokens `IdentityFile` takes, as `ssh` would for `alias`
/// once `config` is resolved. An unknown token is left as written, so the
/// path names no file, as `ssh` would refuse it.
pub(super) fn expand(value: &str, alias: &str, config: &HostConfig, local: &Local) -> String {
    let jump = config.proxy_jump.as_deref().unwrap_or("");
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(token) = chars.next() else {
            out.push('%');
            break;
        };
        match token {
            '%' => out.push('%'),
            'C' => out.push_str(&connection_hash(config, jump, local)),
            'd' => out.push_str(&local.home.to_string_lossy()),
            'h' => out.push_str(&config.hostname),
            'i' => out.push_str(&local.uid.to_string()),
            'j' => out.push_str(jump),
            'k' => out.push_str(config.host_key_alias.as_deref().unwrap_or(alias)),
            'L' => out.push_str(local.host.split('.').next().unwrap_or("")),
            'l' => out.push_str(&local.host),
            'n' => out.push_str(alias),
            'p' => out.push_str(&config.port.to_string()),
            'r' => out.push_str(&config.user),
            'u' => out.push_str(&local.user),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

/// `%C`: a hash of `%l%h%p%r%j`.
fn connection_hash(config: &HostConfig, jump: &str, local: &Local) -> String {
    let joined = format!(
        "{}{}{}{}{jump}",
        local.host, config.hostname, config.port, config.user
    );
    Sha1::digest(joined.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The login name for `uid`, from the password database as `ssh` reads it.
fn user_name(uid: u32) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    // SAFETY: an all-zero passwd is a valid value to be filled in.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found = std::ptr::null_mut();
    // SAFETY: every pointer is to a live local, and `buf`'s length is
    // passed with it; the result points into `buf` and `pwd` only.
    let err = unsafe {
        libc::getpwuid_r(
            uid,
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        )
    };
    if err != 0 || found.is_null() {
        return None;
    }
    // SAFETY: on success `pw_name` is a NUL-terminated string in `buf`.
    let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) };
    Some(name.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> Local {
        Local {
            home: "/home/u".into(),
            user: "me".into(),
            uid: 1000,
            host: "desk.lan".into(),
        }
    }

    fn host() -> HostConfig {
        super::super::parse::parse(
            "hostname real.example.com\nuser bob\nport 2200\nproxyjump jumper\n",
            "web",
            &local(),
            None,
        )
    }

    #[test]
    fn expands_every_token_identityfile_takes() {
        let all = "%n %h %r %p %u %l %L %d %i %k %j %%";
        assert_eq!(
            expand(all, "web", &host(), &local()),
            "web real.example.com bob 2200 me desk.lan desk /home/u 1000 web jumper %"
        );
        let mut aliased = host();
        aliased.host_key_alias = Some("ka".into());
        assert_eq!(expand("%k", "web", &aliased, &local()), "ka");
    }

    #[test]
    fn a_percent_sign_written_twice_is_not_a_token() {
        assert_eq!(expand("a%%d", "web", &host(), &local()), "a%d");
        // Unknown tokens and a trailing `%` stay as written.
        assert_eq!(expand("a%x%", "web", &host(), &local()), "a%x%");
    }

    /// `ssh -G` expands `ControlPath`, which takes the same tokens, so
    /// the two must match for every token, `%C` included.
    #[tokio::test]
    async fn expands_as_the_installed_ssh_does() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config");
        let tokens = "~/k/%n_%h_%r_%p_%u_%l_%L_%d_%i_%k_%j_%C_%%x";
        std::fs::write(
            &config,
            format!(
                "Host web\n  HostName real.example.com\n  User bob\n  Port 2200\n  \
                 ProxyJump jumper\n  IdentityFile {tokens}\n  ControlPath {tokens}\n"
            ),
        )
        .unwrap();
        let output = super::super::ssh_g("web", Some(&config), false)
            .await
            .unwrap();
        let output = String::from_utf8_lossy(&output.stdout);
        let control = output
            .lines()
            .find_map(|l| l.strip_prefix("controlpath "))
            .unwrap();
        let resolved = super::super::resolve("web", Some(&config)).await.unwrap();
        assert_eq!(resolved.identity_files, [PathBuf::from(control)]);
    }
}
