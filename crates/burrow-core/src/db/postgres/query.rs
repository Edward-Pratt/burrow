//! Running the user's SQL on a connected session with the simple query
//! protocol: the rows kept per statement and as JSON, the read-only run
//! the MCP bridge gets, cancelling, and the session's read-only setting.

use std::future::Future;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt;
use tokio::task::JoinHandle;
use tokio_postgres::SimpleQueryMessage;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::tls::NoTls;

use crate::api::{QueryError, QueryResult, StatementResult};
use crate::net::{self};

use super::{Handle, Result, describe};

/// Longer values are cut short in query results. Exports are not.
const MAX_CELL: usize = 64 * 1024;

/// The most a query's kept rows may take as JSON. A reply goes over the
/// daemon's socket as one line, and a line over its limit drops the
/// connection, so a query that gets here is stopped as at the row limit.
/// Counted as JSON rather than as text, since escaping can make a value
/// six times longer.
const MAX_RESULT: usize = 32 << 20;

// The rows fit one reply, with room for column names and the rest.
const _: () = assert!(MAX_RESULT <= crate::ipc::MAX_LINE / 2);

pub(super) fn query_error(error: &tokio_postgres::Error) -> QueryError {
    let Some(db) = error.as_db_error() else {
        return QueryError {
            message: describe(error),
            detail: None,
            hint: None,
            position: None,
            code: None,
        };
    };
    QueryError {
        message: db.message().to_string(),
        detail: db.detail().map(str::to_string),
        hint: db.hint().map(str::to_string),
        position: match db.position() {
            Some(ErrorPosition::Original(position)) => Some(*position),
            _ => None,
        },
        code: Some(db.code().code().to_string()),
    }
}

/// A value for a query result, cut short when huge.
fn cell(text: &str) -> String {
    if text.len() <= MAX_CELL {
        return text.to_string();
    }
    let mut end = MAX_CELL;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// What a row takes in a reply as JSON, with its brackets and separators,
/// counting escapes as serde_json writes them.
fn row_len(values: &[Option<String>]) -> usize {
    let value = |value: &Option<String>| match value {
        None => "null,".len(),
        Some(text) => {
            let escaped: usize = text
                .bytes()
                .map(|b| match b {
                    b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
                    0..0x20 => 6,
                    _ => 1,
                })
                .sum();
            escaped + "\"\",".len()
        }
    };
    "[],".len() + values.iter().map(value).sum::<usize>()
}

/// Whether the session's role can act on the server beyond its data,
/// which a read-only transaction doesn't stop. Membership counts as much as
/// being: a member of a superuser role can `SET ROLE` to it inside a `DO`
/// block. Both users are checked, so an earlier `SET ROLE` changes nothing.
const POWERFUL: &str = "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles r \
     WHERE (r.rolsuper OR r.rolname IN ('pg_execute_server_program', 'pg_write_server_files')) \
     AND (pg_catalog.pg_has_role(session_user, r.oid, 'MEMBER') \
     OR pg_catalog.pg_has_role(current_user, r.oid, 'MEMBER')))";

impl Handle {
    /// Runs `sql`, keeping at most `limit` rows of each statement. A
    /// statement with more makes Burrow cancel the query: a result the
    /// client never reads would otherwise still be sent in full.
    pub async fn query(self: &Arc<Self>, sql: &str, limit: usize) -> QueryResult {
        let _busy = self.busy.lock().await;
        self.query_locked(sql, limit).await
    }

    /// Runs `sql` so that it cannot write, whatever the session's setting:
    /// as one statement, in a read-only transaction that is rolled back.
    ///
    /// The setting alone isn't enough for the MCP bridge, since SQL can
    /// change it (`SET default_transaction_read_only = off; DELETE …`) or
    /// end the transaction (`COMMIT; DELETE …`). Preparing the SQL first
    /// makes the server refuse more than one statement, and one statement
    /// can't both leave the transaction and write.
    ///
    /// A read-only transaction still lets `COPY … TO` write files on the
    /// server and `COPY … TO PROGRAM` run commands there, also from inside
    /// a `DO` block. So a role that may do either, or a superuser, or one
    /// that can become either, is refused: Burrow can't keep it read-only.
    pub async fn query_read_only(self: &Arc<Self>, sql: &str, limit: usize) -> QueryResult {
        let _busy = self.busy.lock().await;
        let started = Instant::now();
        let failed = |error: &tokio_postgres::Error| QueryResult {
            error: Some(query_error(error)),
            elapsed_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            ..QueryResult::default()
        };
        if let Err(e) = self.client.batch_execute("BEGIN READ ONLY").await {
            return failed(&e);
        }
        let powerful = self.client.query_one(POWERFUL, &[]).await;
        let result = match powerful.map(|row| row.get::<_, bool>(0)) {
            Ok(true) => QueryResult {
                error: Some(QueryError {
                    message: "Burrow can't keep this role read-only: it is a superuser, or \
                              may write files or run programs on the server. Connect as a \
                              role without those rights, or allow writing."
                        .to_string(),
                    ..QueryError::default()
                }),
                ..QueryResult::default()
            },
            Ok(false) => match self.client.prepare(sql).await {
                Ok(_) => self.query_locked(sql, limit).await,
                Err(e) => failed(&e),
            },
            Err(e) => failed(&e),
        };
        let _ = self.client.batch_execute("ROLLBACK").await;
        result
    }

    async fn query_locked(self: &Arc<Self>, sql: &str, limit: usize) -> QueryResult {
        let started = Instant::now();
        let mut result = QueryResult::default();
        let mut cancelling: Option<JoinHandle<()>> = None;
        // What the kept rows take as JSON, across all statements.
        let mut bytes = 0;
        let mut full = false;
        match self.client.simple_query_raw(sql).await {
            Err(error) => result.error = Some(query_error(&error)),
            Ok(stream) => {
                let mut stream = pin!(stream);
                let mut current: Option<StatementResult> = None;
                while let Some(message) = stream.next().await {
                    match message {
                        Ok(SimpleQueryMessage::RowDescription(columns)) => {
                            current = Some(StatementResult {
                                columns: columns.iter().map(|c| c.name().to_string()).collect(),
                                ..StatementResult::default()
                            });
                        }
                        Ok(SimpleQueryMessage::Row(row)) => {
                            let statement = current.get_or_insert_with(StatementResult::default);
                            let mut kept = false;
                            if !full && statement.rows.len() < limit {
                                let values: Vec<Option<String>> =
                                    (0..row.len()).map(|i| row.get(i).map(cell)).collect();
                                let size = row_len(&values);
                                // Once one row doesn't fit, none after it is
                                // kept either, even from a later statement
                                // that starts before the cancel arrives.
                                full = bytes + size > MAX_RESULT;
                                if !full {
                                    bytes += size;
                                    statement.rows.push(values);
                                    kept = true;
                                }
                            }
                            if !kept {
                                statement.truncated = true;
                                if cancelling.is_none() {
                                    let handle = self.clone();
                                    cancelling = Some(tokio::spawn(async move {
                                        if let Err(e) = handle.send_cancel().await {
                                            tracing::debug!("could not stop a query: {e}");
                                        }
                                    }));
                                }
                            }
                        }
                        Ok(SimpleQueryMessage::CommandComplete(count)) => {
                            let mut statement = current.take().unwrap_or_default();
                            statement.count = count;
                            result.statements.push(statement);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            if cancelling.is_some()
                                && error.code() == Some(&SqlState::QUERY_CANCELED)
                            {
                                result.stopped = true;
                                if let Some(statement) = current.take() {
                                    result.statements.push(statement);
                                }
                            } else {
                                result.error = Some(query_error(&error));
                            }
                        }
                    }
                }
            }
        }
        // Wait for the cancel to be sent before the next request can start,
        // or a late one could stop that instead.
        if let Some(cancelling) = cancelling {
            let _ = cancelling.await;
        }
        result.elapsed_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        result
    }

    /// Asks the server to stop whatever this session is running.
    pub async fn cancel(&self) -> Result<()> {
        self.send_cancel().await
    }

    /// Boxed with its `Send` spelled out: dialing can sign in, which saves
    /// passwords through `Core::execute`, which runs queries, which cancel.
    /// The compiler can't see through that loop on its own.
    fn send_cancel(&self) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let link = net::dial(&self.core, &self.route, &self.prompter, &|_| {}).await?;
            let sent = match &self.tls {
                Some(tls) => self.cancel.cancel_query_raw(link, tls.clone()).await,
                None => self.cancel.cancel_query_raw(link, NoTls).await,
            };
            sent.map_err(|e| describe(&e))
        })
    }

    pub async fn set_read_only(&self, read_only: bool) -> Result<()> {
        let _busy = self.busy.lock().await;
        let value = if read_only { "on" } else { "off" };
        self.client
            .batch_execute(&format!("SET default_transaction_read_only = {value}"))
            .await
            .map_err(|e| describe(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_values_are_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_CELL);
        let cut = cell(&text);
        assert!(cut.ends_with('…'));
        assert!(cut.len() <= MAX_CELL + '…'.len_utf8());
        assert_eq!(cell("short"), "short");
    }

    #[test]
    fn a_row_is_measured_as_its_json_at_least() {
        let rows = [
            vec![],
            vec![None, Some(String::new())],
            vec![Some("plain é".into()), Some("\"\\\n\t".into())],
            vec![Some("\u{0}\u{1b}\u{7f}".into())],
        ];
        for row in rows {
            let json = serde_json::to_string(&row).unwrap();
            // The count includes a separator the last value doesn't have.
            assert!(row_len(&row) >= json.len(), "{json}");
            assert!(row_len(&row) <= json.len() + 1 + row.len(), "{json}");
        }
        assert_eq!(row_len(&[Some("\u{1}".into())]), "[\"\\u0001\"],".len() + 1);
    }
}
