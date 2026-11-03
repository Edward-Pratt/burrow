//! Exporting every row a query returns to a local CSV file, in a
//! read-only transaction of its own, through a temporary file beside it.

use std::path::Path;
use std::pin::pin;

use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use tokio_postgres::SimpleQueryMessage;

use crate::net::{self};

use super::{Handle, Result, describe};

impl Handle {
    /// Writes every row `sql` returns to `path` as CSV, and says how many
    /// there were. It runs in a read-only transaction of its own, so it
    /// refuses to start inside one the user has open: rolling back at the
    /// end would throw that one's work away.
    pub async fn export(&self, sql: &str, path: &Path) -> Result<u64> {
        net::check_path("path", path)?;
        let dir = path
            .parent()
            .filter(|dir| dir.is_dir())
            .ok_or_else(|| format!("{} is not in a folder that exists", path.display()))?;
        let _busy = self.busy.lock().await;
        // Inside a transaction, statements start later than it did. A
        // simple query, since the extended protocol stamps them apart.
        let check = "SELECT pg_catalog.transaction_timestamp() = pg_catalog.statement_timestamp()";
        let outside = self
            .client
            .simple_query(check)
            .await
            .map_err(|e| describe(&e))?
            .iter()
            .any(|message| match message {
                SimpleQueryMessage::Row(row) => row.get(0) == Some("t"),
                _ => false,
            });
        if !outside {
            return Err("finish the open transaction first: exporting runs in its own".into());
        }
        self.client
            .batch_execute("BEGIN READ ONLY")
            .await
            .map_err(|e| describe(&e))?;
        let written = self.write_csv(sql, dir, path).await;
        let _ = self.client.batch_execute("ROLLBACK").await;
        written
    }

    async fn write_csv(&self, sql: &str, dir: &Path, path: &Path) -> Result<u64> {
        let io = |e: std::io::Error| format!("could not write {}: {e}", path.display());
        // A hidden file next to the target, moved into place when complete.
        // The rename replaces a symlink at `path` rather than following it.
        let temp = tempfile::Builder::new()
            .prefix(".burrow-export-")
            .tempfile_in(dir)
            .map_err(io)?;
        let file = tokio::fs::File::from_std(temp.as_file().try_clone().map_err(io)?);
        let mut out = tokio::io::BufWriter::new(file);
        let mut stream = pin!(
            self.client
                .simple_query_raw(sql)
                .await
                .map_err(|e| describe(&e))?
        );
        let mut rows = 0;
        while let Some(message) = stream.next().await {
            let line = match message.map_err(|e| describe(&e))? {
                SimpleQueryMessage::RowDescription(columns) => {
                    csv_line(columns.iter().map(|c| Some(c.name())))
                }
                SimpleQueryMessage::Row(row) => {
                    rows += 1;
                    csv_line((0..row.len()).map(|i| row.get(i)))
                }
                _ => continue,
            };
            out.write_all(line.as_bytes()).await.map_err(io)?;
        }
        out.flush().await.map_err(io)?;
        out.into_inner().sync_all().await.map_err(io)?;
        temp.persist(path).map_err(|e| io(e.error))?;
        Ok(rows)
    }
}

/// One CSV record. NULL is an empty field and an empty string is `""`, so
/// the two stay apart.
fn csv_line<'a>(values: impl Iterator<Item = Option<&'a str>>) -> String {
    let fields: Vec<String> = values
        .map(|value| match value {
            None => String::new(),
            Some(text) if text.is_empty() || text.contains([',', '"', '\n', '\r']) => {
                format!("\"{}\"", text.replace('"', "\"\""))
            }
            Some(text) => text.to_string(),
        })
        .collect();
    fields.join(",") + "\r\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_keeps_null_and_empty_apart() {
        let line = csv_line([Some("a"), None, Some(""), Some("x,\"y\"\n")].into_iter());
        assert_eq!(line, "a,,\"\",\"x,\"\"y\"\"\n\"\r\n");
    }
}
