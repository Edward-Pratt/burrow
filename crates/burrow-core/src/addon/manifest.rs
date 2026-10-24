//! Reading and checking `addon.toml` files.
//!
//! The `addons/` folder can arrive through a dotfiles repo or a shared
//! folder, so a manifest is untrusted input. It is checked strictly, and a
//! bad one is reported as a problem and skipped rather than half loaded.

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};

use data_encoding::HEXLOWER;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::{
    API_VERSION, AddonAction, AddonCommand, AddonKind, FOLDER_KIND, Problem, SSH_HOST_KIND,
};
use crate::tree::validate_kind;

pub(crate) const MANIFEST_FILE: &str = "addon.toml";

/// The only transport in API version 1: newline-delimited JSON-RPC 2.0 on
/// the program's stdin and stdout.
const PROTOCOL: &str = "jsonrpc-stdio";

/// Kinds the core handles itself, which no addon may claim.
const RESERVED_KINDS: [&str; 2] = [FOLDER_KIND, SSH_HOST_KIND];

/// What an addon may do through the API. This limits the Burrow API only:
/// an addon is a normal program running as the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Permission {
    /// List and read nodes, and receive node events.
    NodesRead,
    /// Create, change, move and delete nodes of the addon's own kinds.
    NodesWrite,
}

impl Permission {
    const ALL: [Permission; 2] = [Permission::NodesRead, Permission::NodesWrite];

    pub fn name(self) -> &'static str {
        match self {
            Permission::NodesRead => "nodes:read",
            Permission::NodesWrite => "nodes:write",
        }
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// `addon.toml` as written. Unknown keys are ignored so manifests written
/// for a newer Burrow still load when they only add optional things.
#[derive(Deserialize)]
struct ManifestFile {
    id: String,
    name: String,
    version: String,
    api: String,
    #[serde(default)]
    permissions: Vec<String>,
    backend: BackendFile,
    #[serde(default)]
    kinds: Vec<AddonKind>,
    #[serde(default)]
    commands: Vec<AddonCommand>,
    #[serde(default)]
    actions: Vec<AddonAction>,
}

#[derive(Deserialize)]
struct BackendFile {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    protocol: String,
}

/// A manifest that passed every check.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub permissions: BTreeSet<Permission>,
    pub kinds: Vec<AddonKind>,
    pub commands: Vec<AddonCommand>,
    pub actions: Vec<AddonAction>,
    /// The addon's folder, canonical. The program runs in it.
    pub dir: PathBuf,
    /// Canonical, and inside `dir`.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// See [`fingerprint`].
    pub fingerprint: String,
}

impl Manifest {
    pub fn has(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }

    pub fn owns(&self, kind: &str) -> bool {
        self.kinds.iter().any(|k| k.kind == kind)
    }
}

/// Loads every `<dir>/<id>/addon.toml`, sorted by id. Folders that fail a
/// check are reported and left out.
pub(crate) fn scan(dir: &Path) -> (Vec<Manifest>, Vec<Problem>) {
    let mut manifests = Vec::new();
    let mut problems = Vec::new();
    let Ok(read) = fs::read_dir(dir) else {
        return (manifests, problems);
    };
    let mut folders: Vec<PathBuf> = read
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
        })
        .collect();
    folders.sort();
    for folder in folders {
        let path = folder.join(MANIFEST_FILE);
        match load(&folder) {
            Ok(manifest) => manifests.push(manifest),
            Err(message) => problems.push(Problem { path, message }),
        }
    }
    (manifests, problems)
}

/// Loads and checks one addon folder.
pub(crate) fn load(folder: &Path) -> Result<Manifest, String> {
    let path = folder.join(MANIFEST_FILE);
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let file: ManifestFile = toml::from_str(&text).map_err(|e| e.message().to_string())?;

    let folder_name = folder.file_name().unwrap_or_default().to_string_lossy();
    check_id("id", &file.id)?;
    if file.id != folder_name {
        return Err(format!(
            "id {:?} must match its folder's name {folder_name:?}",
            file.id
        ));
    }
    check_text("name", &file.name)?;
    check_text("version", &file.version)?;

    let major = file.api.split('.').next().unwrap_or_default();
    if major.parse::<u32>().ok() != Some(API_VERSION) {
        return Err(format!(
            "it needs API {}, but this Burrow has API {API_VERSION}",
            file.api
        ));
    }
    if file.backend.protocol != PROTOCOL {
        return Err(format!(
            "unknown protocol {:?}; only {PROTOCOL:?} is supported",
            file.backend.protocol
        ));
    }

    let mut permissions = BTreeSet::new();
    for name in &file.permissions {
        let permission = Permission::ALL
            .into_iter()
            .find(|p| p.name() == name)
            .ok_or_else(|| format!("unknown permission {name:?}"))?;
        permissions.insert(permission);
    }

    let mut seen = HashSet::new();
    for kind in &file.kinds {
        validate_kind(&kind.kind).map_err(|e| e.to_string())?;
        if RESERVED_KINDS.contains(&kind.kind.as_str()) {
            return Err(format!("kind {:?} belongs to Burrow itself", kind.kind));
        }
        if !seen.insert(&kind.kind) {
            return Err(format!("kind {:?} is listed twice", kind.kind));
        }
        check_text("kind label", &kind.label)?;
        if let Some(icon) = &kind.icon {
            check_id("icon", icon)?;
        }
    }
    let mut seen = HashSet::new();
    for (id, title) in file
        .commands
        .iter()
        .map(|c| (&c.id, &c.title))
        .chain(file.actions.iter().map(|a| (&a.id, &a.title)))
    {
        check_id("command id", id)?;
        check_text("command title", title)?;
        if !seen.insert(id) {
            return Err(format!("command or action {id:?} is listed twice"));
        }
    }
    for action in &file.actions {
        if action.kinds.is_empty() {
            return Err(format!("action {:?} names no kinds", action.id));
        }
        for kind in &action.kinds {
            validate_kind(kind).map_err(|e| e.to_string())?;
        }
    }

    let dir = folder
        .canonicalize()
        .map_err(|e| format!("{}: {e}", folder.display()))?;
    let program = resolve_program(&dir, &file.backend.command)?;
    let files_hash = hash_folder(&dir)?;
    let fingerprint = fingerprint(
        &file.id,
        &permissions,
        &file.kinds,
        &program,
        &file.backend.args,
        &files_hash,
    );
    Ok(Manifest {
        id: file.id,
        name: file.name,
        version: file.version,
        permissions,
        kinds: file.kinds,
        commands: file.commands,
        actions: file.actions,
        dir,
        program,
        args: file.backend.args,
        fingerprint,
    })
}

/// The program must be a relative path to an executable file inside the
/// addon's folder. A bare name is not looked up on `PATH`, and neither `..`
/// nor a symlink can lead out of the folder, so approving an addon covers
/// the file that actually runs.
fn resolve_program(dir: &Path, command: &str) -> Result<PathBuf, String> {
    let relative = Path::new(command);
    let plain = !command.is_empty()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !plain {
        return Err(format!(
            "backend.command {command:?} must be a relative path inside the addon's folder"
        ));
    }
    let program = dir
        .join(relative)
        .canonicalize()
        .map_err(|e| format!("backend.command {command:?}: {e}"))?;
    if !program.starts_with(dir) {
        return Err(format!(
            "backend.command {command:?} leads outside the addon's folder"
        ));
    }
    let meta = fs::metadata(&program).map_err(|e| format!("{}: {e}", program.display()))?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "backend.command {command:?} is not an executable file"
        ));
    }
    Ok(program)
}

/// Limits on what an approval can pin. A folder past them is refused
/// rather than partly pinned.
const MAX_FILES: usize = 4096;
const MAX_BYTES: u64 = 256 << 20;

/// A hash of every file in the addon's folder, by relative path, except
/// `addon.toml` (its approved fields are pinned separately). The program
/// is often a script that loads its siblings, so pinning the program file
/// alone would let those change unseen. Symlinks are pinned by their target
/// and refused when they lead out of the folder.
fn hash_folder(dir: &Path) -> Result<String, String> {
    let mut entries = Vec::new();
    let mut bytes = 0;
    walk(dir, dir, &mut entries, &mut bytes)?;
    entries.sort();
    let mut hasher = Sha256::new();
    for (path, hash) in entries {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    Ok(HEXLOWER.encode(&hasher.finalize()))
}

fn walk(
    root: &Path,
    dir: &Path,
    entries: &mut Vec<(String, String)>,
    bytes: &mut u64,
) -> Result<(), String> {
    let read = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in read {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        if relative == MANIFEST_FILE {
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(root, &path, entries, bytes)?;
            continue;
        }
        if entries.len() >= MAX_FILES {
            return Err(format!("the folder has over {MAX_FILES} files"));
        }
        let hash = if meta.is_symlink() {
            let leads_out = path.canonicalize().map_or(true, |p| !p.starts_with(root));
            if leads_out {
                return Err(format!("{relative} links outside the addon's folder"));
            }
            let target = fs::read_link(&path).map_err(|e| format!("{relative}: {e}"))?;
            format!("link:{}", target.display())
        } else if meta.is_file() {
            *bytes += meta.len();
            if *bytes > MAX_BYTES {
                return Err(format!("the folder holds over {} MiB", MAX_BYTES >> 20));
            }
            let contents = fs::read(&path).map_err(|e| format!("{relative}: {e}"))?;
            HEXLOWER.encode(&Sha256::digest(&contents))
        } else {
            // Sockets and pipes have no contents to pin.
            continue;
        };
        entries.push((relative, hash));
    }
    Ok(())
}

/// What an approval covers. Titles, the name and the version are left out,
/// so rewording a menu entry does not ask for approval again.
#[derive(Serialize)]
struct Approved<'a> {
    id: &'a str,
    permissions: Vec<&'static str>,
    kinds: Vec<&'a str>,
    program: &'a Path,
    args: &'a [String],
    files_sha256: &'a str,
}

fn fingerprint(
    id: &str,
    permissions: &BTreeSet<Permission>,
    kinds: &[AddonKind],
    program: &Path,
    args: &[String],
    files_hash: &str,
) -> String {
    let mut kinds: Vec<&str> = kinds.iter().map(|k| k.kind.as_str()).collect();
    kinds.sort_unstable();
    let approved = Approved {
        id,
        permissions: permissions.iter().map(|p| p.name()).collect(),
        kinds,
        program,
        args,
        files_sha256: files_hash,
    };
    let json = serde_json::to_vec(&approved).expect("plain data always serializes");
    HEXLOWER.encode(&Sha256::digest(&json))
}

/// Ids and icon names: lowercase letters, digits, `-` and `.`.
fn check_id(field: &str, value: &str) -> Result<(), String> {
    let ok = !value.is_empty()
        && !value.starts_with(['.', '-'])
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{field} {value:?} must be lowercase letters, digits, '-' or '.'"
        ))
    }
}

fn check_text(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        Err(format!(
            "{field} {value:?} must be non-empty text on one line"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    const MANIFEST: &str = r#"
id = "notes"
name = "Notes"
version = "0.1.0"
api = "1"
permissions = ["nodes:read"]

[backend]
command = "bin/notes"
protocol = "jsonrpc-stdio"

[[kinds]]
kind = "note"
label = "Note"
"#;

    fn addon(manifest: &str) -> (TempDir, PathBuf) {
        let root = TempDir::new().unwrap();
        let folder = root.path().join("notes");
        fs::create_dir_all(folder.join("bin")).unwrap();
        fs::write(folder.join(MANIFEST_FILE), manifest).unwrap();
        let program = folder.join("bin/notes");
        fs::write(&program, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        (root, folder)
    }

    fn error(manifest: &str) -> String {
        let (_root, folder) = addon(manifest);
        load(&folder).unwrap_err()
    }

    #[test]
    fn loads_a_good_manifest() {
        let (_root, folder) = addon(MANIFEST);
        let manifest = load(&folder).unwrap();
        assert_eq!(manifest.id, "notes");
        assert!(manifest.has(Permission::NodesRead));
        assert!(!manifest.has(Permission::NodesWrite));
        assert!(manifest.owns("note"));
        assert_eq!(
            manifest.program,
            folder.canonicalize().unwrap().join("bin/notes")
        );
    }

    #[test]
    fn refuses_bad_fields() {
        assert!(error(&MANIFEST.replace(r#"id = "notes""#, r#"id = "other""#)).contains("folder"));
        assert!(error(&MANIFEST.replace(r#"api = "1""#, r#"api = "2""#)).contains("API 2"));
        assert!(error(&MANIFEST.replace("jsonrpc-stdio", "grpc")).contains("protocol"));
        assert!(error(&MANIFEST.replace("nodes:read", "db")).contains("unknown permission"));
        assert!(
            error(&MANIFEST.replace(r#"kind = "note""#, r#"kind = "ssh-host""#))
                .contains("belongs to Burrow")
        );
        assert!(error(&MANIFEST.replace(r#"kind = "note""#, r#"kind = "No Te""#)).contains("kind"));
    }

    #[test]
    fn program_stays_inside_the_folder() {
        for command in ["/bin/sh", "../notes/bin/notes", "sh", ""] {
            let message = error(&MANIFEST.replace("bin/notes", command));
            assert!(message.contains("backend.command"), "{command}: {message}");
        }

        let (root, folder) = addon(&MANIFEST.replace("bin/notes", "bin/link"));
        let outside = root.path().join("outside");
        fs::write(&outside, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&outside, folder.join("bin/link")).unwrap();
        assert!(load(&folder).unwrap_err().contains("outside"));
    }

    #[test]
    fn links_out_of_the_folder_are_refused() {
        let (root, folder) = addon(MANIFEST);
        let outside = root.path().join("secret.py");
        fs::write(&outside, "print()\n").unwrap();
        symlink(&outside, folder.join("lib.py")).unwrap();
        assert!(load(&folder).unwrap_err().contains("links outside"));
    }

    #[test]
    fn program_must_be_executable() {
        let (_root, folder) = addon(MANIFEST);
        fs::set_permissions(folder.join("bin/notes"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load(&folder).unwrap_err().contains("not an executable"));
    }

    #[test]
    fn fingerprint_follows_what_is_approved() {
        let (_root, folder) = addon(MANIFEST);
        let first = load(&folder).unwrap().fingerprint;

        let renamed = MANIFEST.replace(r#"name = "Notes""#, r#"name = "My notes""#);
        fs::write(folder.join(MANIFEST_FILE), renamed).unwrap();
        assert_eq!(load(&folder).unwrap().fingerprint, first);

        fs::write(folder.join("bin/notes"), "#!/bin/sh\necho changed\n").unwrap();
        let changed = load(&folder).unwrap().fingerprint;
        assert_ne!(changed, first);

        // Any file the program might load counts, not just the program.
        fs::write(folder.join("bin/helper.sh"), "echo hi\n").unwrap();
        let added = load(&folder).unwrap().fingerprint;
        assert_ne!(added, changed);
        let changed = added;

        let more = MANIFEST.replace(r#"["nodes:read"]"#, r#"["nodes:read", "nodes:write"]"#);
        fs::write(folder.join(MANIFEST_FILE), more).unwrap();
        assert_ne!(load(&folder).unwrap().fingerprint, changed);
    }
}
