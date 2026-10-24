//! The sidebar tree and its on-disk layout.
//!
//! Each folder is a directory with an optional `_folder.toml`; each leaf is a
//! `<slug>.toml` file. A node's parent is the directory it sits in, so moving
//! a file in a file manager moves the node. Every file carries an `id`; files
//! without one (made by hand) get one written back on load.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::api::{FOLDER_KIND, Node, NodeId, NodePatch, Problem};
use crate::error::{Error, Result};

pub(crate) const FOLDER_FILE: &str = "_folder.toml";
const LEAF_EXT: &str = "toml";

/// What a node file looks like on disk. Unknown keys are ignored, so files
/// written by a newer Burrow still load.
#[derive(Default, Serialize, Deserialize)]
struct NodeFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<NodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    props: toml::Table,
    /// Field name to secret reference, e.g. `password = "keyring:burrow/…"`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    secrets: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Entry {
    pub node: Node,
    /// The leaf's file, or the folder's directory.
    pub path: PathBuf,
    pub secret_refs: BTreeMap<String, String>,
}

pub(crate) struct Tree {
    dir: PathBuf,
    entries: HashMap<NodeId, Entry>,
}

impl Tree {
    /// Loads the tree under `dir`, creating `dir` if needed. Files that fail
    /// to parse are skipped and reported as problems.
    pub fn load(dir: &Path) -> Result<(Tree, Vec<Problem>)> {
        fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
        let mut tree = Tree {
            dir: dir.to_path_buf(),
            entries: HashMap::new(),
        };
        let mut problems = Vec::new();
        tree.load_dir(dir, None, &mut problems);
        Ok((tree, problems))
    }

    fn load_dir(&mut self, dir: &Path, parent: Option<NodeId>, problems: &mut Vec<Problem>) {
        let read = match fs::read_dir(dir) {
            Ok(read) => read,
            Err(e) => {
                problems.push(problem(dir, e));
                return;
            }
        };
        // Sorted so that duplicate ids resolve the same way on every load.
        let mut paths: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();

        for path in paths {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            // Hidden files include our own in-progress atomic writes.
            if name.starts_with('.') || name == FOLDER_FILE {
                continue;
            }
            if path.is_dir() {
                self.load_folder(&path, parent, problems);
            } else if path.extension().is_some_and(|ext| ext == LEAF_EXT) {
                self.load_leaf(&path, parent, problems);
            }
        }
    }

    fn load_folder(&mut self, dir: &Path, parent: Option<NodeId>, problems: &mut Vec<Problem>) {
        let meta = dir.join(FOLDER_FILE);
        let (file, parsed) = if meta.exists() {
            match read_node_file(&meta) {
                Ok(file) => (file, true),
                Err(p) => {
                    problems.push(p);
                    (NodeFile::default(), false)
                }
            }
        } else {
            (NodeFile::default(), true)
        };

        let mut needs_write = !meta.exists();
        let id = self.claim_id(file.id, &mut needs_write);
        let entry = Entry {
            node: Node {
                id,
                parent,
                kind: FOLDER_KIND.to_string(),
                name: file.name.unwrap_or_else(|| file_label(dir)),
                color: file.color,
                props: file.props,
                secrets: file.secrets.keys().cloned().collect(),
            },
            path: dir.to_path_buf(),
            secret_refs: file.secrets,
        };
        // Never overwrite a file we failed to parse: the user may be halfway
        // through editing it. The folder gets a temporary id instead.
        if needs_write
            && parsed
            && let Err(e) = write_entry(&entry)
        {
            problems.push(problem(&meta, e));
        }
        self.entries.insert(id, entry);
        self.load_dir(dir, Some(id), problems);
    }

    fn load_leaf(&mut self, path: &Path, parent: Option<NodeId>, problems: &mut Vec<Problem>) {
        let file = match read_node_file(path) {
            Ok(file) => file,
            Err(p) => {
                problems.push(p);
                return;
            }
        };
        let kind = match file.kind {
            Some(kind) if kind == FOLDER_KIND => {
                problems.push(problem(
                    path,
                    "kind \"folder\" is only valid in _folder.toml",
                ));
                return;
            }
            Some(kind) if !kind.is_empty() => kind,
            _ => {
                problems.push(problem(path, "missing `kind`"));
                return;
            }
        };

        let mut needs_write = false;
        let id = self.claim_id(file.id, &mut needs_write);
        let entry = Entry {
            node: Node {
                id,
                parent,
                kind,
                name: file.name.unwrap_or_else(|| file_label(path)),
                color: file.color,
                props: file.props,
                secrets: file.secrets.keys().cloned().collect(),
            },
            path: path.to_path_buf(),
            secret_refs: file.secrets,
        };
        if needs_write && let Err(e) = write_entry(&entry) {
            problems.push(problem(path, e));
        }
        self.entries.insert(id, entry);
    }

    /// Keeps the file's id unless it is missing or already taken (a copied
    /// file), in which case a fresh one is assigned and must be written.
    fn claim_id(&self, id: Option<NodeId>, needs_write: &mut bool) -> NodeId {
        match id {
            Some(id) if !self.entries.contains_key(&id) => id,
            _ => {
                *needs_write = true;
                NodeId::new()
            }
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn entries(&self) -> &HashMap<NodeId, Entry> {
        &self.entries
    }

    pub fn get(&self, id: NodeId) -> Result<&Entry> {
        self.entries.get(&id).ok_or(Error::NodeNotFound(id))
    }

    /// All nodes, sorted by name so clients get a stable order.
    pub fn nodes(&self) -> Vec<Node> {
        let mut nodes: Vec<Node> = self.entries.values().map(|e| e.node.clone()).collect();
        nodes.sort_by_key(|a| (a.name.to_lowercase(), a.id));
        nodes
    }

    /// Number of folders above `id`.
    pub fn depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.entries.get(&id).and_then(|e| e.node.parent);
        while let Some(parent) = current {
            depth += 1;
            current = self.entries.get(&parent).and_then(|e| e.node.parent);
        }
        depth
    }

    pub fn create(
        &mut self,
        parent: Option<NodeId>,
        kind: String,
        name: String,
        color: Option<String>,
        props: toml::Table,
    ) -> Result<Node> {
        validate_kind(&kind)?;
        let name = validate_name(name)?;
        validate_color(color.as_deref())?;
        let dir = self.folder_dir(parent)?;

        let node = Node {
            id: NodeId::new(),
            parent,
            kind,
            name,
            color,
            props,
            secrets: Default::default(),
        };
        let path = if node.is_folder() {
            let path = unique_path(&dir, &slug(&node.name), None, None);
            fs::create_dir(&path).map_err(|e| Error::io(&path, e))?;
            path
        } else {
            unique_path(&dir, &slug(&node.name), Some(LEAF_EXT), None)
        };
        let entry = Entry {
            node: node.clone(),
            path,
            secret_refs: Default::default(),
        };
        write_entry(&entry)?;
        self.entries.insert(node.id, entry);
        Ok(node)
    }

    pub fn update(&mut self, id: NodeId, patch: NodePatch) -> Result<Node> {
        let old = self.get(id)?.clone();
        let mut new = old.clone();
        if let Some(name) = patch.name {
            new.node.name = validate_name(name)?;
        }
        if let Some(color) = patch.color {
            validate_color(color.as_deref())?;
            new.node.color = color;
        }
        if let Some(props) = patch.props {
            new.node.props = props;
        }

        // Keep file names in step with node names.
        if new.node.name != old.node.name {
            let dir = parent_dir(&old.path);
            let target = unique_path(
                &dir,
                &slug(&new.node.name),
                extension(&old),
                Some(&old.path),
            );
            if target != old.path {
                fs::rename(&old.path, &target).map_err(|e| Error::io(&old.path, e))?;
                self.reprefix(&old.path, &target);
                new.path = target;
            }
        }
        write_entry(&new)?;
        let node = new.node.clone();
        self.entries.insert(id, new);
        Ok(node)
    }

    pub fn move_node(&mut self, id: NodeId, parent: Option<NodeId>) -> Result<Node> {
        let old = self.get(id)?.clone();
        if old.node.parent == parent {
            return Ok(old.node);
        }
        if let Some(parent) = parent
            && self.is_within(parent, id)
        {
            return Err(Error::MoveIntoSelf);
        }
        let dir = self.folder_dir(parent)?;
        let stem = if old.node.is_folder() {
            file_label(&old.path)
        } else {
            old.path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| slug(&old.node.name))
        };
        let target = unique_path(&dir, &stem, extension(&old), None);
        fs::rename(&old.path, &target).map_err(|e| Error::io(&old.path, e))?;
        self.reprefix(&old.path, &target);

        let entry = self.entries.get_mut(&id).expect("entry exists");
        entry.node.parent = parent;
        Ok(entry.node.clone())
    }

    /// Removes a node, and for folders everything inside it. Returns the
    /// removed entries, deepest first.
    pub fn delete(&mut self, id: NodeId, recursive: bool) -> Result<Vec<Entry>> {
        let entry = self.get(id)?.clone();
        let mut inside: Vec<NodeId> = self
            .entries
            .keys()
            .copied()
            .filter(|&other| other != id && self.is_within(other, id))
            .collect();
        if !inside.is_empty() && !recursive {
            return Err(Error::FolderNotEmpty(id));
        }

        if entry.node.is_folder() {
            fs::remove_dir_all(&entry.path).map_err(|e| Error::io(&entry.path, e))?;
        } else {
            fs::remove_file(&entry.path).map_err(|e| Error::io(&entry.path, e))?;
        }

        inside.sort_by_key(|&other| std::cmp::Reverse(self.depth(other)));
        inside.push(id);
        Ok(inside
            .into_iter()
            .filter_map(|id| self.entries.remove(&id))
            .collect())
    }

    /// Sets or clears the reference for one secret field.
    pub fn set_secret_ref(
        &mut self,
        id: NodeId,
        field: &str,
        reference: Option<String>,
    ) -> Result<Node> {
        let mut entry = self.get(id)?.clone();
        match reference {
            Some(reference) => {
                entry.secret_refs.insert(field.to_string(), reference);
                entry.node.secrets.insert(field.to_string());
            }
            None => {
                entry.secret_refs.remove(field);
                entry.node.secrets.remove(field);
            }
        }
        write_entry(&entry)?;
        let node = entry.node.clone();
        self.entries.insert(id, entry);
        Ok(node)
    }

    /// The directory that children of `parent` live in.
    fn folder_dir(&self, parent: Option<NodeId>) -> Result<PathBuf> {
        match parent {
            None => Ok(self.dir.clone()),
            Some(id) => {
                let entry = self.get(id)?;
                if entry.node.is_folder() {
                    Ok(entry.path.clone())
                } else {
                    Err(Error::NotAFolder(id))
                }
            }
        }
    }

    /// True when `id` is `ancestor` or sits somewhere inside it.
    fn is_within(&self, id: NodeId, ancestor: NodeId) -> bool {
        let mut current = Some(id);
        while let Some(node) = current {
            if node == ancestor {
                return true;
            }
            current = self.entries.get(&node).and_then(|e| e.node.parent);
        }
        false
    }

    /// After a rename on disk, points every path under `old` at `new`.
    fn reprefix(&mut self, old: &Path, new: &Path) {
        for entry in self.entries.values_mut() {
            if let Ok(rest) = entry.path.strip_prefix(old) {
                entry.path = if rest.as_os_str().is_empty() {
                    new.to_path_buf()
                } else {
                    new.join(rest)
                };
            }
        }
    }
}

fn read_node_file(path: &Path) -> Result<NodeFile, Problem> {
    let text = fs::read_to_string(path).map_err(|e| problem(path, e))?;
    toml::from_str(&text).map_err(|e| problem(path, e.message()))
}

fn write_entry(entry: &Entry) -> Result<()> {
    let folder = entry.node.is_folder();
    let file = NodeFile {
        id: Some(entry.node.id),
        kind: (!folder).then(|| entry.node.kind.clone()),
        name: Some(entry.node.name.clone()),
        color: entry.node.color.clone(),
        props: entry.node.props.clone(),
        secrets: entry.secret_refs.clone(),
    };
    let target = if folder {
        entry.path.join(FOLDER_FILE)
    } else {
        entry.path.clone()
    };
    let text = toml::to_string_pretty(&file).map_err(|e| Error::Encode {
        path: target.clone(),
        message: e.to_string(),
    })?;
    write_atomic(&target, &text)
}

/// Writes to a hidden temp file in the same directory, then renames it over
/// the target, so readers (including our own watcher) never see half a file.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = parent_dir(path);
    let mut tmp = NamedTempFile::new_in(&dir).map_err(|e| Error::io(&dir, e))?;
    tmp.write_all(contents.as_bytes())
        .and_then(|()| tmp.as_file().sync_all())
        .map_err(|e| Error::io(tmp.path(), e))?;
    tmp.persist(path).map_err(|e| Error::io(path, e.error))?;
    Ok(())
}

/// The first free path `dir/stem[-N][.ext]`. `current` counts as free so a
/// node can keep its own name.
fn unique_path(dir: &Path, stem: &str, ext: Option<&str>, current: Option<&Path>) -> PathBuf {
    (1..)
        .map(|n| {
            let base = if n == 1 {
                stem.to_string()
            } else {
                format!("{stem}-{n}")
            };
            match ext {
                Some(ext) => dir.join(format!("{base}.{ext}")),
                None => dir.join(base),
            }
        })
        .find(|path| Some(path.as_path()) == current || !path.exists())
        .expect("an unbounded range always finds a free name")
}

/// A file-name-safe version of a node name: lowercase, with runs of other
/// characters turned into single dashes.
fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "node".to_string()
    } else {
        out
    }
}

fn extension(entry: &Entry) -> Option<&'static str> {
    (!entry.node.is_folder()).then_some(LEAF_EXT)
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent().map(Path::to_path_buf).unwrap_or_default()
}

/// A file or directory name without its extension, used as a default name.
fn file_label(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn problem(path: &Path, message: impl ToString) -> Problem {
    Problem {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

fn validate_kind(kind: &str) -> Result<()> {
    let ok = !kind.is_empty()
        && kind
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "kind",
            format!("{kind:?} must be lowercase letters, digits, '-' or '.'"),
        ))
    }
}

fn validate_name(name: String) -> Result<String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err(Error::invalid("name", "must not be empty"));
    }
    if name.chars().any(char::is_control) {
        return Err(Error::invalid(
            "name",
            "must not contain control characters",
        ));
    }
    Ok(name)
}

fn validate_color(color: Option<&str>) -> Result<()> {
    let Some(color) = color else { return Ok(()) };
    let ok = color.strip_prefix('#').is_some_and(|hex| {
        matches!(hex.len(), 3 | 6) && hex.chars().all(|c| c.is_ascii_hexdigit())
    });
    if ok {
        Ok(())
    } else {
        Err(Error::invalid(
            "color",
            format!("{color:?} is not #rgb or #rrggbb"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Prod Web (EU)"), "prod-web-eu");
        assert_eq!(slug("  --  "), "node");
        assert_eq!(slug("Ünïcode ok"), "ünïcode-ok");
    }

    #[test]
    fn colors() {
        assert!(validate_color(Some("#e06c75")).is_ok());
        assert!(validate_color(Some("#fff")).is_ok());
        assert!(validate_color(Some("red")).is_err());
        assert!(validate_color(Some("#12345")).is_err());
        assert!(validate_color(None).is_ok());
    }

    #[test]
    fn kinds() {
        assert!(validate_kind("ssh-host").is_ok());
        assert!(validate_kind("postgres.connection").is_ok());
        assert!(validate_kind("SSH").is_err());
        assert!(validate_kind("").is_err());
    }
}
