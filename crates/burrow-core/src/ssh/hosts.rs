//! Hosts added in Burrow.
//!
//! Each one is appended as a `Host` block to `config.d/burrow.conf` beside
//! the OpenSSH config, and one `Include` line at the top of that config
//! pulls the file in, so `ssh <alias>` in a shell connects the way Burrow
//! does. Burrow never rewrites a line in either file, and `ssh -G` stays the
//! only reader of both: whether an alias is taken, and whether the new block
//! applies, is asked of `ssh -v -G`.

use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::ssh::config::{self, Trace};
use crate::tree::write_atomic_mode;

const HEADER: &str = "# Hosts added in Burrow. Burrow only ever appends to this file.\n";
const INCLUDE_COMMENT: &str = "# Added by Burrow, so ssh reads the hosts added in Burrow.\n";

/// What the user typed for a new host. Only the alias and host name are
/// required.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct NewHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
}

/// Where hosts added in Burrow go.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Files {
    /// The OpenSSH config the `Include` line goes into.
    pub config: PathBuf,
    /// `config.d/burrow.conf` beside it.
    pub hosts: PathBuf,
    /// The line that pulls `hosts` in.
    pub include: String,
}

/// Whether the OpenSSH config already reads the hosts file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct IncludeStatus {
    pub included: bool,
    pub config: PathBuf,
    pub line: String,
}

/// A host block that was written, which [`Added::undo`] takes back.
#[must_use]
pub(crate) struct Added {
    hosts: PathBuf,
    before: String,
    after: String,
}

impl Added {
    /// Puts the file back as it was, unless someone changed it since.
    pub fn undo(self) {
        match fs::read_to_string(&self.hosts) {
            Ok(now) if now == self.after => {
                if let Err(e) = write_atomic_mode(&self.hosts, &self.before, 0o600) {
                    tracing::warn!("could not take back the host just added: {e}");
                }
            }
            _ => tracing::warn!(
                "{} changed after Burrow added a host to it; left as it is",
                self.hosts.display()
            ),
        }
    }
}

impl Files {
    /// `~/.ssh/config`, or `config_file` when Burrow was pointed at another
    /// one, as `ssh -F` does.
    pub fn locate(config_file: Option<&Path>, home: &Path) -> Result<Files> {
        let Some(file) = config_file else {
            return Ok(Files {
                config: home.join(".ssh/config"),
                hosts: home.join(".ssh/config.d/burrow.conf"),
                include: "Include ~/.ssh/config.d/burrow.conf".into(),
            });
        };
        // OpenSSH reads a relative Include from ~/.ssh whichever config it
        // is in, so another config gets an absolute path.
        let config = std::path::absolute(file).map_err(|e| Error::io(file, e))?;
        let dir = config
            .parent()
            .ok_or_else(|| Error::invalid("OpenSSH config", "it has no folder"))?;
        let hosts = dir.join("config.d/burrow.conf");
        let text = hosts.to_string_lossy();
        // Include expands globs and quotes as well as `~`.
        if let Some(c) = text
            .chars()
            .find(|c| c.is_control() || "\"'\\*?[".contains(*c))
        {
            return Err(Error::invalid(
                "OpenSSH config",
                format!("its folder name has {c:?} in it, which an Include line can't hold"),
            ));
        }
        let include = format!("Include {}", quoted(&text));
        Ok(Files {
            config,
            hosts,
            include,
        })
    }
}

impl NewHost {
    /// Every value ends up in a file `ssh` reads, where a space, quote,
    /// `#`, `%` token or pattern character would change its meaning, so
    /// each field takes only the characters it needs. `check_alias` alone
    /// would allow `*`, and a `Host *` block read first would take over
    /// every host.
    pub fn check(&self) -> Result<()> {
        check_new_alias(&self.alias)?;
        plain(
            "host name",
            &self.hostname,
            "letters, digits, '.', '-' and ':'",
            |c| c.is_ascii_alphanumeric() || ".-:".contains(c),
        )?;
        if let Some(user) = &self.user {
            plain(
                "user",
                user,
                "letters, digits, '.', '_', '@' and '-'",
                |c| c.is_ascii_alphanumeric() || "._@-".contains(c),
            )?;
        }
        if self.port == Some(0) {
            return Err(Error::invalid("port", "it must be from 1 to 65535"));
        }
        if let Some(key) = &self.identity_file {
            if !key.is_absolute() {
                return Err(Error::invalid(
                    "key",
                    format!("{} is not an absolute path", key.display()),
                ));
            }
            if !fs::metadata(key).is_ok_and(|m| m.is_file()) {
                return Err(Error::invalid(
                    "key",
                    format!("there is no key file at {}", key.display()),
                ));
            }
        }
        Ok(())
    }

    /// The `Host` block, after [`NewHost::check`] passed.
    fn render(&self, home: &Path) -> Result<String> {
        let mut block = format!("Host {}\n    HostName {}\n", self.alias, self.hostname);
        if let Some(user) = &self.user {
            block += &format!("    User {user}\n");
        }
        if let Some(port) = self.port {
            block += &format!("    Port {port}\n");
        }
        if let Some(key) = &self.identity_file {
            block += &format!("    IdentityFile {}\n", key_arg(key, home)?);
        }
        Ok(block)
    }
}

fn check_new_alias(alias: &str) -> Result<()> {
    config::check_alias(alias).map_err(|r| Error::invalid("alias", r))?;
    plain("alias", alias, "letters, digits, '.', '_' and '-'", |c| {
        c.is_ascii_alphanumeric() || "._-".contains(c)
    })
}

fn plain(field: &'static str, value: &str, allowed: &str, ok: impl Fn(char) -> bool) -> Result<()> {
    if value.is_empty() {
        return Err(Error::invalid(field, "it is empty"));
    }
    if value.len() > 255 {
        return Err(Error::invalid(field, "it is longer than 255 characters"));
    }
    if value.starts_with('-') {
        return Err(Error::invalid(
            field,
            format!("{value:?} must not start with '-'"),
        ));
    }
    if !value.chars().all(ok) {
        return Err(Error::invalid(
            field,
            format!("{value:?} may only contain {allowed}"),
        ));
    }
    Ok(())
}

/// A key path as `IdentityFile` reads it: `~/` for the home folder, `%%`
/// for a literal `%`, and quoted when it has spaces. Quotes, backslashes
/// and `${` have no safe spelling there, so they are refused.
fn key_arg(key: &Path, home: &Path) -> Result<String> {
    let text = match key.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => key.display().to_string(),
    };
    if text.contains("${") || text.chars().any(|c| c.is_control() || "\"'\\".contains(c)) {
        return Err(Error::invalid(
            "key",
            format!(
                "{} has a quote, backslash, '${{' or control character in it, \
                 which an OpenSSH config can't hold",
                key.display()
            ),
        ));
    }
    Ok(quoted(&text.replace('%', "%%")))
}

fn quoted(text: &str) -> String {
    if text.contains(|c: char| c.is_whitespace() || c == '#') {
        format!("\"{text}\"")
    } else {
        text.to_string()
    }
}

/// Reports whether `ssh` reads the hosts file when it looks up `alias`,
/// after the same checks of the alias as [`add`], so a client asks about
/// the Include line only for a host it can add. Creates the hosts file
/// when missing, since `ssh` only reports reading files that exist.
pub(crate) async fn include_status(
    config_file: Option<&Path>,
    alias: &str,
) -> Result<IncludeStatus> {
    check_new_alias(alias)?;
    let home = home()?;
    let files = Files::locate(config_file, &home)?;
    ensure_hosts_file(&files)?;
    let trace = free(alias, config_file, &files, &home).await?;
    Ok(IncludeStatus {
        included: reads(&trace, &files.hosts),
        config: files.config,
        line: files.include,
    })
}

/// Appends `host` to the hosts file, adding the `Include` line first when
/// `add_include` allows it. Refuses an alias that is already set anywhere,
/// and checks afterwards that `ssh` now applies the new block, undoing it
/// otherwise.
pub(crate) async fn add(
    config_file: Option<&Path>,
    host: &NewHost,
    add_include: bool,
) -> Result<Added> {
    host.check()?;
    let home = home()?;
    let files = Files::locate(config_file, &home)?;
    let block = host.render(&home)?;
    let before = ensure_hosts_file(&files)?;
    let alias = host.alias.as_str();
    let everywhere = free(alias, config_file, &files, &home).await?;
    if !reads(&everywhere, &files.hosts) {
        if !add_include {
            return Err(Error::invalid(
                "include",
                format!(
                    "{} doesn't read {} yet; adding a host needs the line {:?} at its top",
                    shown(&files.config, &home),
                    shown(&files.hosts, &home),
                    files.include
                ),
            ));
        }
        add_include_line(&files, &home)?;
    }

    let after = format!("{before}{}{block}", separator(&before));
    write_atomic_mode(&files.hosts, &after, 0o600)?;
    let added = Added {
        hosts: files.hosts.clone(),
        before,
        after,
    };
    let applies = match trace(alias, config_file).await {
        Ok(trace) => trace
            .naming(alias)
            .is_some_and(|found| same_file(&found.file, &files.hosts))
            .then_some(())
            .ok_or_else(|| inactive(&trace, &files, &home)),
        Err(e) => Err(e),
    };
    match applies {
        Ok(()) => Ok(added),
        Err(e) => {
            added.undo();
            Err(e)
        }
    }
}

/// Refuses an alias that a `Host` line already names, and returns the
/// trace of the whole config. The hosts file is traced on its own as well,
/// since until the Include is in place (or while it sits inside a Host
/// block) the config's trace doesn't see its blocks.
async fn free(
    alias: &str,
    config_file: Option<&Path>,
    files: &Files,
    home: &Path,
) -> Result<Trace> {
    let everywhere = trace(alias, config_file).await?;
    let own = trace(alias, Some(&files.hosts)).await?;
    if let Some(found) = everywhere.naming(alias).or(own.naming(alias)) {
        return Err(taken(alias, found, files, home));
    }
    Ok(everywhere)
}

async fn trace(alias: &str, config_file: Option<&Path>) -> Result<Trace> {
    config::trace(alias, config_file)
        .await
        .map_err(|e| Error::invalid("OpenSSH config", e))
}

fn home() -> Result<PathBuf> {
    std::env::home_dir()
        .filter(|h| h.is_absolute())
        .ok_or_else(|| Error::invalid("home folder", "could not find it"))
}

fn reads(trace: &Trace, file: &Path) -> bool {
    trace.read.iter().any(|read| same_file(read, file))
}

/// OpenSSH prints paths as it expanded them, which may reach the same file
/// another way, such as through a symlinked home folder.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

fn taken(alias: &str, found: &config::Applied, files: &Files, home: &Path) -> Error {
    let place = format!("{} line {}", shown(&found.file, home), found.line);
    let message = if same_file(&found.file, &files.hosts) {
        format!(
            "{alias} was already added in Burrow ({place}). \
             Choose to use a host already in your SSH config to add it as it is"
        )
    } else {
        format!(
            "{alias} is already in your SSH config ({place}). \
             Choose to use a host already in your SSH config, or pick another alias"
        )
    };
    Error::invalid("alias", message)
}

/// Why a block that was written doesn't apply.
fn inactive(trace: &Trace, files: &Files, home: &Path) -> Error {
    let config = shown(&files.config, home);
    let hosts = shown(&files.hosts, home);
    let message = if reads(trace, &files.hosts) {
        format!(
            "{config} reads {hosts} inside a Host or Match block, so its hosts don't apply. \
             Move its Include line above the first Host line"
        )
    } else {
        format!(
            "{config} doesn't read {hosts}; add {:?} at its top",
            files.include
        )
    };
    Error::invalid("include", message)
}

/// The blank line between blocks.
fn separator(before: &str) -> &'static str {
    if before.is_empty() || before.ends_with("\n\n") {
        ""
    } else if before.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    }
}

/// `~/…` for paths in the home folder, as people know them.
fn shown(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn refuse_symlink(path: &Path, meta: &fs::Metadata, home: &Path) -> Result<()> {
    if meta.file_type().is_symlink() {
        return Err(Error::invalid(
            "OpenSSH config",
            format!(
                "{} is a symlink, which Burrow won't write through",
                shown(path, home)
            ),
        ));
    }
    Ok(())
}

/// Creates the hosts file and its folders when missing, and returns what
/// the file holds now. Neither the folder nor the file may be a symlink:
/// renaming over one would replace it.
fn ensure_hosts_file(files: &Files) -> Result<String> {
    let home = home()?;
    let dir = files
        .hosts
        .parent()
        .ok_or_else(|| Error::invalid("OpenSSH config", "it has no folder"))?;
    let ssh_dir = dir.parent().unwrap_or(dir);
    // ssh refuses a config in a folder others can write to.
    let private_dir = || {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
    };
    private_dir()
        .create(ssh_dir)
        .map_err(|e| Error::io(ssh_dir, e))?;
    match fs::symlink_metadata(dir) {
        Ok(meta) => {
            refuse_symlink(dir, &meta, &home)?;
            if !meta.is_dir() {
                return Err(Error::invalid(
                    "OpenSSH config",
                    format!("{} is not a folder", shown(dir, &home)),
                ));
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            private_dir().create(dir).map_err(|e| Error::io(dir, e))?;
        }
        Err(e) => return Err(Error::io(dir, e)),
    }
    match fs::symlink_metadata(&files.hosts) {
        Ok(meta) => {
            refuse_symlink(&files.hosts, &meta, &home)?;
            if !meta.is_file() {
                return Err(Error::invalid(
                    "OpenSSH config",
                    format!("{} is not a file", shown(&files.hosts, &home)),
                ));
            }
            fs::read_to_string(&files.hosts).map_err(|e| Error::io(&files.hosts, e))
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {
            write_atomic_mode(&files.hosts, HEADER, 0o600)?;
            Ok(HEADER.to_string())
        }
        Err(e) => Err(Error::io(&files.hosts, e)),
    }
}

/// Puts the `Include` line at the very top of the config: after a `Host`
/// line it would only apply to that host. The file keeps its permissions.
fn add_include_line(files: &Files, home: &Path) -> Result<()> {
    let (existing, mode) = match fs::symlink_metadata(&files.config) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Error::invalid(
                "include",
                format!(
                    "{} is a symlink, which Burrow won't write through. \
                     Add this line at its top yourself: {}",
                    shown(&files.config, home),
                    files.include
                ),
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(Error::invalid(
                "include",
                format!("{} is not a file", shown(&files.config, home)),
            ));
        }
        Ok(meta) => (
            fs::read_to_string(&files.config).map_err(|e| Error::io(&files.config, e))?,
            meta.permissions().mode() & 0o7777,
        ),
        Err(e) if e.kind() == ErrorKind::NotFound => (String::new(), 0o600),
        Err(e) => return Err(Error::io(&files.config, e)),
    };
    let gap = if existing.is_empty() { "" } else { "\n" };
    let text = format!("{INCLUDE_COMMENT}{}\n{gap}{existing}", files.include);
    write_atomic_mode(&files.config, &text, mode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn host(alias: &str) -> NewHost {
        NewHost {
            alias: alias.into(),
            hostname: "10.0.0.5".into(),
            ..Default::default()
        }
    }

    fn invalid_field(result: Result<impl std::fmt::Debug>) -> &'static str {
        match result {
            Err(Error::Invalid { field, .. }) => field,
            other => panic!("expected an invalid field, got {other:?}"),
        }
    }

    #[test]
    fn plain_values_pass() {
        let dir = TempDir::new().unwrap();
        let key = dir.path().join("id_ed25519");
        fs::write(&key, "key").unwrap();
        let full = NewHost {
            alias: "prod-web.1_a".into(),
            hostname: "fe80::1".into(),
            user: Some("deploy@corp.example".into()),
            port: Some(2222),
            identity_file: Some(key),
        };
        full.check().unwrap();
        host("web").check().unwrap();
    }

    #[test]
    fn values_that_would_change_the_config_are_refused() {
        let alias = |a: &str| invalid_field(host(a).check());
        // `Host *` read first would take over every host.
        for bad in [
            "*", "web*", "w?b", "!web", "web,db", "a b", "-oX", "%h", "", "w\"b",
        ] {
            assert_eq!(alias(bad), "alias", "{bad:?}");
        }
        assert_eq!(alias(&"a".repeat(256)), "alias");
        for bad in ["", "-x", "a b", "h\nProxyCommand x", "%h", "h#c", "a\"b"] {
            let host = NewHost {
                hostname: bad.into(),
                ..host("web")
            };
            assert_eq!(invalid_field(host.check()), "host name", "{bad:?}");
        }
        for bad in ["", "-u", "a b", "%u", "a\"b", "root\nHost *"] {
            let host = NewHost {
                user: Some(bad.into()),
                ..host("web")
            };
            assert_eq!(invalid_field(host.check()), "user", "{bad:?}");
        }
        let zero = NewHost {
            port: Some(0),
            ..host("web")
        };
        assert_eq!(invalid_field(zero.check()), "port");
    }

    #[test]
    fn a_key_must_be_an_existing_file_at_an_absolute_path() {
        let dir = TempDir::new().unwrap();
        let check = |path: PathBuf| {
            NewHost {
                identity_file: Some(path),
                ..host("web")
            }
            .check()
        };
        assert_eq!(invalid_field(check("id_rsa".into())), "key");
        assert_eq!(invalid_field(check(dir.path().join("missing"))), "key");
        assert_eq!(invalid_field(check(dir.path().into())), "key");
    }

    #[test]
    fn blocks_hold_only_what_was_given() {
        let home = Path::new("/home/u");
        assert_eq!(
            host("web").render(home).unwrap(),
            "Host web\n    HostName 10.0.0.5\n"
        );
        let full = NewHost {
            user: Some("deploy".into()),
            port: Some(2222),
            identity_file: Some("/home/u/.ssh/id_ed25519".into()),
            ..host("web")
        };
        assert_eq!(
            full.render(home).unwrap(),
            "Host web\n    HostName 10.0.0.5\n    User deploy\n    Port 2222\n    \
             IdentityFile ~/.ssh/id_ed25519\n"
        );
    }

    #[test]
    fn key_paths_are_written_so_ssh_reads_them_literally() {
        let home = Path::new("/home/u");
        let arg = |p: &str| key_arg(Path::new(p), home);
        assert_eq!(arg("/home/u/.ssh/k").unwrap(), "~/.ssh/k");
        assert_eq!(arg("/opt/keys/k").unwrap(), "/opt/keys/k");
        // `%d` would expand to the home folder.
        assert_eq!(arg("/home/u/100%d").unwrap(), "~/100%%d");
        assert_eq!(arg("/home/u/my keys/k").unwrap(), "\"~/my keys/k\"");
        assert_eq!(arg("/home/u/a#b").unwrap(), "\"~/a#b\"");
        for bad in ["/home/u/${HOME}", "/a\"b", "/a'b", "/a\\b", "/a\nb"] {
            assert!(arg(bad).is_err(), "{bad:?}");
        }
        // A home folder that is only a prefix of the name isn't the home.
        assert_eq!(arg("/home/user2/k").unwrap(), "/home/user2/k");
    }

    #[test]
    fn files_sit_beside_the_config() {
        let home = Path::new("/home/u");
        let default = Files::locate(None, home).unwrap();
        assert_eq!(default.config, Path::new("/home/u/.ssh/config"));
        assert_eq!(
            default.hosts,
            Path::new("/home/u/.ssh/config.d/burrow.conf")
        );
        assert_eq!(default.include, "Include ~/.ssh/config.d/burrow.conf");

        let other = Files::locate(Some(Path::new("/dev runs/ssh_config")), home).unwrap();
        assert_eq!(other.hosts, Path::new("/dev runs/config.d/burrow.conf"));
        assert_eq!(other.include, "Include \"/dev runs/config.d/burrow.conf\"");
        assert!(Files::locate(Some(Path::new("/a*b/config")), home).is_err());
    }

    #[test]
    fn blocks_are_separated_by_one_blank_line() {
        assert_eq!(separator(""), "");
        assert_eq!(separator(HEADER), "\n");
        assert_eq!(separator("Host a\n\n"), "");
        assert_eq!(separator("Host a"), "\n\n");
    }

    fn files_in(dir: &Path) -> Files {
        Files::locate(Some(&dir.join("config")), Path::new("/nowhere")).unwrap()
    }

    #[test]
    fn the_hosts_file_is_created_private() {
        let dir = TempDir::new().unwrap();
        let files = files_in(&dir.path().join(".ssh"));
        assert_eq!(ensure_hosts_file(&files).unwrap(), HEADER);
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join(".ssh")), 0o700);
        assert_eq!(mode(files.hosts.parent().unwrap()), 0o700);
        assert_eq!(mode(&files.hosts), 0o600);
        // A second call reads what is there.
        fs::write(&files.hosts, "Host a\n").unwrap();
        assert_eq!(ensure_hosts_file(&files).unwrap(), "Host a\n");
    }

    #[test]
    fn symlinks_are_never_written_through() {
        let dir = TempDir::new().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        fs::create_dir(&elsewhere).unwrap();

        let linked_dir = dir.path().join("a");
        fs::create_dir(&linked_dir).unwrap();
        symlink(&elsewhere, linked_dir.join("config.d")).unwrap();
        assert!(ensure_hosts_file(&files_in(&linked_dir)).is_err());

        let linked_file = dir.path().join("b");
        fs::create_dir_all(linked_file.join("config.d")).unwrap();
        fs::write(elsewhere.join("target"), "x").unwrap();
        symlink(
            elsewhere.join("target"),
            linked_file.join("config.d/burrow.conf"),
        )
        .unwrap();
        assert!(ensure_hosts_file(&files_in(&linked_file)).is_err());

        let linked_config = dir.path().join("c");
        fs::create_dir(&linked_config).unwrap();
        symlink(elsewhere.join("target"), linked_config.join("config")).unwrap();
        let files = files_in(&linked_config);
        let error = add_include_line(&files, Path::new("/nowhere")).unwrap_err();
        assert!(error.to_string().contains(&files.include), "{error}");
        assert_eq!(fs::read_to_string(elsewhere.join("target")).unwrap(), "x");
    }

    #[test]
    fn the_include_goes_on_top_and_keeps_the_rest() {
        let dir = TempDir::new().unwrap();
        let files = files_in(dir.path());
        let original = "Host old\n    Port 2\n";
        fs::write(&files.config, original).unwrap();
        fs::set_permissions(&files.config, fs::Permissions::from_mode(0o640)).unwrap();
        add_include_line(&files, Path::new("/nowhere")).unwrap();
        let text = fs::read_to_string(&files.config).unwrap();
        assert_eq!(
            text,
            format!("{INCLUDE_COMMENT}{}\n\n{original}", files.include)
        );
        let mode = fs::metadata(&files.config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn a_missing_config_gets_just_the_include() {
        let dir = TempDir::new().unwrap();
        let files = files_in(dir.path());
        add_include_line(&files, Path::new("/nowhere")).unwrap();
        let text = fs::read_to_string(&files.config).unwrap();
        assert_eq!(text, format!("{INCLUDE_COMMENT}{}\n", files.include));
    }

    #[test]
    fn undo_leaves_a_file_changed_since() {
        let dir = TempDir::new().unwrap();
        let hosts = dir.path().join("burrow.conf");
        fs::write(&hosts, "before\nadded\n").unwrap();
        let added = |after: &str| Added {
            hosts: hosts.clone(),
            before: "before\n".into(),
            after: after.into(),
        };
        added("something else").undo();
        assert_eq!(fs::read_to_string(&hosts).unwrap(), "before\nadded\n");
        added("before\nadded\n").undo();
        assert_eq!(fs::read_to_string(&hosts).unwrap(), "before\n");
    }

    /// A config folder for the real `ssh`, as `BURROW_SSH_CONFIG` gives.
    struct Setup {
        _dir: TempDir,
        config: PathBuf,
        files: Files,
    }

    impl Setup {
        fn new(config: &str) -> Setup {
            let dir = TempDir::new().unwrap();
            let path = dir.path().join("ssh_config");
            let files = Files::locate(Some(&path), Path::new("/nowhere")).unwrap();
            let folder = files.hosts.parent().unwrap().to_string_lossy();
            let config = config
                .replace("HOSTS_DIR", &folder)
                .replace("HOSTS", &files.hosts.to_string_lossy());
            // `ssh -F` needs the file to exist, as `~/.ssh/config` doesn't.
            fs::write(&path, config).unwrap();
            Setup {
                _dir: dir,
                config: path,
                files,
            }
        }

        async fn add(&self, host: &NewHost, add_include: bool) -> Result<()> {
            add(Some(&self.config), host, add_include).await.map(drop)
        }

        fn hosts(&self) -> String {
            fs::read_to_string(&self.files.hosts).unwrap()
        }
    }

    #[tokio::test]
    async fn a_new_host_needs_the_include_then_resolves() {
        let setup = Setup::new("");
        let key = setup.config.with_file_name("id_ed25519");
        fs::write(&key, "key").unwrap();
        let web = NewHost {
            user: Some("deploy".into()),
            port: Some(2222),
            identity_file: Some(key.clone()),
            ..host("web")
        };

        let status = include_status(Some(&setup.config), "web").await.unwrap();
        assert!(!status.included);
        assert_eq!(status.line, setup.files.include);
        let refused = setup.add(&web, false).await;
        assert_eq!(invalid_field(refused), "include");
        assert_eq!(setup.hosts(), HEADER);
        assert_eq!(fs::read_to_string(&setup.config).unwrap(), "");

        setup.add(&web, true).await.unwrap();
        let resolved = config::resolve("web", Some(&setup.config)).await.unwrap();
        assert_eq!(resolved.hostname, "10.0.0.5");
        assert_eq!(resolved.user, "deploy");
        assert_eq!(resolved.port, 2222);
        assert_eq!(resolved.identity_files, [key]);
        let status = include_status(Some(&setup.config), "db").await.unwrap();
        assert!(status.included);

        // The second host needs no question, and the first stays.
        setup.add(&host("db"), false).await.unwrap();
        assert_eq!(
            setup.hosts().matches("Host ").count(),
            2,
            "{}",
            setup.hosts()
        );
    }

    #[tokio::test]
    async fn an_alias_set_elsewhere_is_refused_with_its_line() {
        let setup = Setup::new("Include HOSTS\n\nHost db\nHost web\n    Port 2\n");
        let error = setup.add(&host("web"), true).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("ssh_config line 4"), "{text}");
        assert!(text.contains("already in your SSH config"), "{text}");
        assert_eq!(setup.hosts(), HEADER);
        // Checking first says so too, so nobody is asked about the
        // Include line for a host that can't be added.
        let status = include_status(Some(&setup.config), "web").await;
        assert_eq!(invalid_field(status), "alias");
        let status = include_status(Some(&setup.config), "*").await;
        assert_eq!(invalid_field(status), "alias");
    }

    #[tokio::test]
    async fn an_alias_added_before_is_refused_even_before_the_include() {
        let setup = Setup::new("");
        fs::create_dir_all(setup.files.hosts.parent().unwrap()).unwrap();
        fs::write(&setup.files.hosts, "Host web\n    HostName a\n").unwrap();
        let error = setup.add(&host("web"), true).await.unwrap_err();
        assert!(
            error.to_string().contains("already added in Burrow"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&setup.config).unwrap(), "");
    }

    #[tokio::test]
    async fn wildcards_do_not_make_an_alias_taken() {
        let setup = Setup::new("Host *.corp\n    User wild\nHost *\n    Port 2\n");
        setup.add(&host("db.corp"), true).await.unwrap();
        // The new block sets what it has; the wildcards fill in the rest.
        let resolved = config::resolve("db.corp", Some(&setup.config))
            .await
            .unwrap();
        assert_eq!(resolved.hostname, "10.0.0.5");
        assert_eq!(resolved.user, "wild");
        assert_eq!(resolved.port, 2);
    }

    #[tokio::test]
    async fn an_include_of_the_whole_folder_needs_no_question() {
        let setup = Setup::new("Include HOSTS_DIR/*\n");
        let config = fs::read_to_string(&setup.config).unwrap();
        let status = include_status(Some(&setup.config), "web").await.unwrap();
        assert!(status.included, "{config}");
        setup.add(&host("web"), false).await.unwrap();
        assert_eq!(fs::read_to_string(&setup.config).unwrap(), config);
    }

    #[tokio::test]
    async fn an_include_inside_a_host_block_is_reported_and_undone() {
        let setup = Setup::new("Host other\n    Port 5\nInclude HOSTS\n");
        let error = setup.add(&host("web"), true).await.unwrap_err();
        assert!(
            error.to_string().contains("inside a Host or Match block"),
            "{error}"
        );
        assert_eq!(setup.hosts(), HEADER);
    }
}
