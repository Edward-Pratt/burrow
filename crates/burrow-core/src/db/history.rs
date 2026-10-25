//! The queries run on each database node, kept in the state folder on this
//! computer. Queries can hold data and even passwords, so the files are
//! private to the user and never travel with the config folder.

use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::api::{HistoryEntry, NodeId};
use crate::error::{Error, Result};
use crate::tree::write_atomic;

/// Entries kept per node; older ones are dropped.
const KEEP: usize = 200;

/// Longer queries, such as a pasted dump, are run but not kept.
const MAX_SQL: usize = 64 * 1024;

pub(crate) struct History {
    dir: PathBuf,
    /// Held while a file is read and rewritten.
    lock: Mutex<()>,
}

#[derive(Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    entries: Vec<HistoryEntry>,
}

impl History {
    pub fn new(dir: PathBuf) -> History {
        History {
            dir,
            lock: Mutex::default(),
        }
    }

    fn path(&self, node: NodeId) -> PathBuf {
        self.dir.join(format!("{node}.json"))
    }

    /// A node's queries, oldest first. A missing or broken file is empty.
    pub fn list(&self, node: NodeId) -> Vec<HistoryEntry> {
        let _lock = self.lock.lock().unwrap();
        self.read(node).entries
    }

    fn read(&self, node: NodeId) -> Stored {
        std::fs::read_to_string(self.path(node))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Adds a query, or moves it to the end if it is already there.
    pub fn record(&self, node: NodeId, sql: &str, ok: bool) -> Result<()> {
        let sql = sql.trim();
        if sql.is_empty() || sql.len() > MAX_SQL {
            return Ok(());
        }
        let _lock = self.lock.lock().unwrap();
        let mut stored = self.read(node);
        stored.entries.retain(|e| e.sql != sql);
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        stored.entries.push(HistoryEntry {
            sql: sql.to_string(),
            at,
            ok,
        });
        let excess = stored.entries.len().saturating_sub(KEEP);
        stored.entries.drain(..excess);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|e| Error::io(&self.dir, e))?;
        let text = serde_json::to_string(&stored).map_err(|e| Error::Encode {
            path: self.path(node),
            message: e.to_string(),
        })?;
        // A temporary file renamed into place, so the file is 0600.
        write_atomic(&self.path(node), &text)
    }

    /// Forgets the queries of every node in `nodes`.
    pub fn forget(&self, nodes: &[NodeId]) {
        let _lock = self.lock.lock().unwrap();
        for &node in nodes {
            let path = self.path(node);
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("could not remove {}: {e}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn keeps_recent_queries_once_each_in_a_private_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let history = History::new(dir.path().join("history"));
        let node = NodeId::new();
        history.record(node, "select 1", true).unwrap();
        history.record(node, "select nope", false).unwrap();
        history.record(node, "  select 1\n", true).unwrap();
        let sql: Vec<_> = history.list(node).into_iter().map(|e| e.sql).collect();
        assert_eq!(sql, ["select nope", "select 1"]);

        let mode = |path: PathBuf| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(history.path(node)), 0o600);
        assert_eq!(mode(dir.path().join("history")), 0o700);

        history.forget(&[node]);
        assert!(history.list(node).is_empty());
    }

    #[test]
    fn drops_the_oldest_beyond_its_limit_and_skips_huge_queries() {
        let dir = tempfile::TempDir::new().unwrap();
        let history = History::new(dir.path().to_path_buf());
        let node = NodeId::new();
        for n in 0..KEEP + 5 {
            history.record(node, &format!("select {n}"), true).unwrap();
        }
        history
            .record(node, &"x".repeat(MAX_SQL + 1), true)
            .unwrap();
        let entries = history.list(node);
        assert_eq!(entries.len(), KEEP);
        assert_eq!(entries[0].sql, "select 5");
    }
}
