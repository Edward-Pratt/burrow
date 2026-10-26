//! PostgreSQL connections: a node's props become a [`Route`] and sign-in
//! settings, and a connected session runs the user's SQL with the simple
//! query protocol, so every value comes back as the text the server prints.
//!
//! The node's file is untrusted, so its props are checked before anything
//! is dialed. A socket is named by its directory and port, as libpq does,
//! so a file can't point Burrow at some other socket. A saved password is
//! pinned to the user, database, route and TLS settings it was typed for:
//! the server can ask for it in clear text, so a `git pull` that turns TLS
//! off, or sends the connection through another host, makes Burrow ask
//! again rather than send it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use futures_util::StreamExt;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::{ErrorPosition, SqlState};
use tokio_postgres::tls::{ChannelBinding, NoTls, TlsConnect};
use tokio_postgres::{CancelToken, Config, SimpleQueryMessage};

use crate::api::{
    DbColumn, DbSchema, DbTable, DbTableKind, Node, NodeId, PromptAnswer, PromptKind, QueryError,
    QueryResult, StatementResult,
};
use crate::engine::Core;
use crate::net::tls::{self, Verify};
use crate::net::{self, Link, Route, Target};
use crate::password::{Saved, save_password, saved_password};
use crate::ssh::Prompter;

type Result<T> = std::result::Result<T, String>;

const DEFAULT_PORT: u16 = 5432;

/// Where Arch, Fedora and Debian (through `/var/run`) put the server's
/// socket.
const DEFAULT_SOCKET_DIR: &str = "/run/postgresql";

/// Wrong passwords allowed before giving up, as for SSH.
const ATTEMPTS: usize = 3;

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

/// What a database node's props describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    pub route: Route,
    pub user: String,
    pub database: String,
    pub sslmode: String,
    pub tls: Option<TlsSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TlsSpec {
    pub verify: Verify,
    pub root_cert: Option<PathBuf>,
    /// The name the certificate is checked against: the host as dialed.
    pub name: String,
}

impl Spec {
    /// Reads and checks a database node's props:
    ///
    /// ```toml
    /// via = "<id of an ssh-host node>"   # optional
    /// host = "db.internal"               # TCP; as the SSH host sees it
    /// socket = "/run/postgresql"         # or a socket directory
    /// port = 5432                        # optional
    /// user = "app"
    /// database = "app"                   # optional; the user's name
    /// sslmode = "verify-full"            # optional
    /// sslrootcert = "/path/to/ca.pem"    # optional
    /// ```
    ///
    /// Without `host` the server is reached through its socket. `sslmode`
    /// is `disable`, `require`, `verify-ca` or `verify-full`. It defaults to
    /// `verify-full` for a server reached straight over the network, and to
    /// `disable` through an SSH host or a socket, which are already private.
    pub fn from_node(node: &Node) -> Result<Spec> {
        let props = &node.props;
        let string = |key: &str| -> Result<Option<&str>> {
            match props.get(key) {
                None => Ok(None),
                Some(value) => value
                    .as_str()
                    .map(Some)
                    .ok_or_else(|| format!("{key} must be text")),
            }
        };
        let port = match props.get("port") {
            None => DEFAULT_PORT,
            Some(value) => value
                .as_integer()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
                .ok_or("port must be a port number from 1 to 65535")?,
        };
        let via = string("via")?
            .map(|via| {
                via.parse()
                    .map(NodeId)
                    .map_err(|_| "via must be the id of an SSH host".to_string())
            })
            .transpose()?;
        let target = match (string("host")?, string("socket")?) {
            (Some(_), Some(_)) => return Err("set host or socket, not both".into()),
            (Some(host), None) => Target::tcp("host", host, port)?,
            (None, socket) => {
                let dir = PathBuf::from(socket.unwrap_or(DEFAULT_SOCKET_DIR));
                net::check_path("socket", &dir)?;
                Target::unix(dir.join(format!(".s.PGSQL.{port}")))?
            }
        };
        let user = string("user")?.ok_or("user is missing")?;
        check_name("user", user)?;
        let database = string("database")?.unwrap_or(user);
        check_name("database", database)?;

        let direct_tcp = via.is_none() && matches!(target, Target::Tcp { .. });
        let sslmode =
            string("sslmode")?.unwrap_or(if direct_tcp { "verify-full" } else { "disable" });
        let verify = match sslmode {
            "disable" => None,
            "require" => Some(Verify::Nothing),
            "verify-ca" => Some(Verify::Ca),
            "verify-full" => Some(Verify::Full),
            other => {
                return Err(format!(
                    "sslmode {other:?} is not disable, require, verify-ca or verify-full"
                ));
            }
        };
        let tls = match (verify, &target) {
            (None, _) => None,
            (Some(_), Target::Unix(_)) => {
                return Err("TLS isn't used over a socket: set sslmode = \"disable\"".into());
            }
            (Some(verify), Target::Tcp { host, .. }) => {
                let root_cert = string("sslrootcert")?.map(PathBuf::from);
                if let Some(path) = &root_cert {
                    net::check_path("sslrootcert", path)?;
                }
                Some(TlsSpec {
                    verify,
                    root_cert,
                    name: host.clone(),
                })
            }
        };
        Ok(Spec {
            route: Route { via, target },
            user: user.to_string(),
            database: database.to_string(),
            sslmode: sslmode.to_string(),
            tls,
        })
    }

    /// Where a password goes: everything that decides who could read it.
    /// `host` is the SSH host it goes through. Each part is quoted, so no
    /// user or database name can make one target read as another.
    fn password_target(&self, host: Option<&Node>) -> String {
        let mut target = format!(
            "{:?}@{:?}/{:?}",
            self.user,
            self.route.target.to_string(),
            self.database
        );
        if let Some(host) = host {
            let alias = crate::ssh::alias(host);
            target += &format!(" through {alias:?} ({})", host.id);
        }
        target += &format!(", sslmode={:?}", self.sslmode);
        if let Some(path) = self.tls.as_ref().and_then(|t| t.root_cert.as_ref()) {
            target += &format!(", sslrootcert={:?}", path.display().to_string());
        }
        target
    }
}

/// A user or database name: Postgres allows nearly anything, but never a
/// NUL, and control characters only make trouble in prompts.
fn check_name(field: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
        return Err(format!(
            "{field} {name:?} must be 1 to 63 bytes without control characters"
        ));
    }
    Ok(())
}

/// A connected database session.
pub(crate) struct Handle {
    client: tokio_postgres::Client,
    cancel: CancelToken,
    route: Route,
    tls: Option<Rustls>,
    core: Core,
    /// Dials the route again to cancel a query. Its connection is already
    /// open by then, so it asks nothing.
    prompter: Prompter,
    /// Held for each request, so a query's rows and a cancel can't be
    /// mixed up with another request's.
    busy: tokio::sync::Mutex<()>,
}

/// Connects and signs in. The returned task ends, saying why, when the
/// connection does.
pub(crate) async fn connect(
    core: &Core,
    node: &Node,
    spec: &Spec,
    prompter: &Prompter,
    step: &impl Fn(String),
) -> Result<(Handle, JoinHandle<String>)> {
    let host = spec
        .route
        .via
        .map(|via| net::ssh_host(core, via))
        .transpose()?;
    let tls = spec.tls.as_ref().map(Rustls::new).transpose()?;
    let target = spec.password_target(host.as_ref());
    let saved = saved_password(core, node, &target).await;
    let mut password = match &saved {
        Saved::Matches(password) => Some(password.clone()),
        _ => None,
    };
    let mut save = false;
    let mut asked = 0;
    loop {
        let link = net::dial(core, &spec.route, prompter, step).await?;
        step(format!("Signing in as {}", spec.user));
        let mut config = Config::new();
        config
            .user(&spec.user)
            .dbname(&spec.database)
            .application_name("Burrow")
            .options("-c default_transaction_read_only=on")
            .ssl_mode(match tls {
                Some(_) => SslMode::Require,
                None => SslMode::Disable,
            });
        if let Some(password) = &password {
            config.password(password.expose());
        }
        let attempt = match &tls {
            Some(tls) => start(&config, link, tls.clone()).await,
            None => start(&config, link, NoTls).await,
        };
        let error = match attempt {
            Ok((client, task)) => {
                if save && let Some(password) = password {
                    save_password(core, node, password, target).await;
                }
                let handle = Handle {
                    cancel: client.cancel_token(),
                    client,
                    route: spec.route.clone(),
                    tls,
                    core: core.clone(),
                    prompter: prompter.clone(),
                    busy: tokio::sync::Mutex::default(),
                };
                return Ok((handle, task));
            }
            Err(error) => error,
        };
        if !wants_password(&error) || asked == ATTEMPTS {
            return Err(refused(&error, tls.is_some()));
        }
        let message = match (&saved, &password) {
            (_, Some(_)) => format!("That didn't work. Password for {target}"),
            (Saved::ForElsewhere(other), None) => format!(
                "The saved password is for {other}, so Burrow didn't send it. \
                 Password for {target}"
            ),
            _ => format!("Password for {target}"),
        };
        asked += 1;
        match prompter
            .ask(PromptKind::Secret { can_save: true }, "Password", &message)
            .await
        {
            PromptAnswer::Text { value, save: keep } => {
                password = Some(value);
                save = keep;
            }
            _ => return Err("signing in was cancelled".into()),
        }
    }
}

async fn start<T>(
    config: &Config,
    link: Link,
    tls: T,
) -> std::result::Result<(tokio_postgres::Client, JoinHandle<String>), tokio_postgres::Error>
where
    T: TlsConnect<Link>,
    T::Stream: Send + 'static,
{
    let (client, connection) = config.connect_raw(link, tls).await?;
    let task = tokio::spawn(async move {
        match connection.await {
            Ok(()) => "the server closed the connection".to_string(),
            Err(e) => describe(&e),
        }
    });
    Ok((client, task))
}

/// Why connecting failed. TLS is on by default over the network, so a
/// server without it, such as a fresh local one, is the likeliest first
/// failure, and the user needs to know what to change.
fn refused(error: &tokio_postgres::Error, tls: bool) -> String {
    let text = describe(error);
    if tls && text.contains("server does not support TLS") {
        return format!(
            "{text}. Turn TLS off for this database only if the network to it is private."
        );
    }
    text
}

/// Whether signing in failed for want of the right password. tokio-postgres
/// says "password missing" when the server asks for one and it has none.
fn wants_password(error: &tokio_postgres::Error) -> bool {
    error.code() == Some(&SqlState::INVALID_PASSWORD)
        || describe(error).contains("password missing")
}

/// The server's message, or tokio-postgres's with its causes: on its own it
/// can be as vague as "invalid configuration".
fn describe(error: &tokio_postgres::Error) -> String {
    if let Some(db) = error.as_db_error() {
        return db.message().to_string();
    }
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        text += &format!(": {cause}");
        source = cause.source();
    }
    text
}

fn query_error(error: &tokio_postgres::Error) -> QueryError {
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

    pub async fn schema(&self) -> Result<Vec<DbSchema>> {
        let _busy = self.busy.lock().await;
        let rows = |sql: &'static str| async move {
            self.client.query(sql, &[]).await.map_err(|e| describe(&e))
        };
        let mut schemas: Vec<DbSchema> = rows(SCHEMAS)
            .await?
            .iter()
            .map(|row| DbSchema {
                name: row.get(0),
                tables: Vec::new(),
            })
            .collect();
        for row in rows(RELATIONS).await? {
            let schema: String = row.get(0);
            let Some(schema) = schemas.iter_mut().find(|s| s.name == schema) else {
                continue;
            };
            let table: String = row.get(1);
            if schema.tables.last().is_none_or(|t| t.name != table) {
                let kind = match row.get::<_, String>(2).as_str() {
                    "v" => DbTableKind::View,
                    "m" => DbTableKind::MaterializedView,
                    "f" => DbTableKind::ForeignTable,
                    "p" => DbTableKind::PartitionedTable,
                    _ => DbTableKind::Table,
                };
                schema.tables.push(DbTable {
                    name: table,
                    kind,
                    columns: Vec::new(),
                });
            }
            if let Some(name) = row.get::<_, Option<String>>(3) {
                let table = schema.tables.last_mut().expect("pushed above");
                table.columns.push(DbColumn {
                    name,
                    data_type: row.get(4),
                    nullable: row.get(5),
                });
            }
        }
        Ok(schemas)
    }

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

/// Schemas the user made, leaving out the system's own.
const SCHEMAS: &str = "\
    SELECT nspname::text FROM pg_catalog.pg_namespace \
    WHERE nspname <> 'information_schema' AND nspname !~ '^pg_' \
    ORDER BY 1";

/// Their tables, views and other relations, with columns in order. A
/// relation without columns still has a row, with NULL for the column.
const RELATIONS: &str = "\
    SELECT n.nspname::text, c.relname::text, c.relkind::text, a.attname::text, \
           pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull \
    FROM pg_catalog.pg_class c \
    JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
    LEFT JOIN pg_catalog.pg_attribute a \
      ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped \
    WHERE c.relkind IN ('r', 'v', 'm', 'f', 'p') \
      AND n.nspname <> 'information_schema' AND n.nspname !~ '^pg_' \
    ORDER BY 1, 2, a.attnum";

/// TLS through rustls, for tokio-postgres's in-band negotiation.
#[derive(Clone)]
struct Rustls {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl Rustls {
    fn new(spec: &TlsSpec) -> Result<Rustls> {
        Ok(Rustls {
            config: tls::client_config(spec.verify, spec.root_cert.as_deref())?,
            name: ServerName::try_from(spec.name.clone())
                .map_err(|_| format!("{:?} can't be checked against a certificate", spec.name))?,
        })
    }
}

impl<S> TlsConnect<S> for Rustls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = TlsStream<S>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<TlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let connector = tokio_rustls::TlsConnector::from(self.config);
            Ok(TlsStream(connector.connect(self.name, stream).await?))
        })
    }
}

struct TlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> tokio_postgres::tls::TlsStream for TlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for TlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(kind: &str, props: &str) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            kind: kind.into(),
            name: "db".into(),
            color: None,
            props: toml::from_str(props).unwrap(),
            secrets: Default::default(),
        }
    }

    fn spec(props: &str) -> Result<Spec> {
        Spec::from_node(&node("postgres", props))
    }

    #[test]
    fn a_host_is_reached_over_tcp_with_full_verification() {
        let spec = spec("host = \"db.example\"\nuser = \"app\"").unwrap();
        assert_eq!(spec.route.via, None);
        assert_eq!(spec.route.target.to_string(), "db.example:5432");
        assert_eq!(spec.database, "app");
        assert_eq!(spec.sslmode, "verify-full");
        assert_eq!(spec.tls.unwrap().verify, Verify::Full);
    }

    #[test]
    fn without_a_host_the_socket_is_named_from_its_directory_and_port() {
        let spec = spec("user = \"me\"\nport = 5433").unwrap();
        assert_eq!(
            spec.route.target,
            Target::Unix("/run/postgresql/.s.PGSQL.5433".into())
        );
        assert_eq!(spec.tls, None);
        let spec = self::spec("user = \"me\"\nsocket = \"/tmp/pg\"").unwrap();
        assert_eq!(
            spec.route.target,
            Target::Unix("/tmp/pg/.s.PGSQL.5432".into())
        );
    }

    #[test]
    fn through_an_ssh_host_tls_is_off_unless_asked_for() {
        let via = "via = \"6f1c2a4e-0000-4000-8000-000000000000\"\nuser = \"app\"";
        let spec = spec(&format!("{via}\nhost = \"localhost\"")).unwrap();
        assert!(spec.route.via.is_some());
        assert_eq!(spec.tls, None);
        let spec = self::spec(&format!(
            "{via}\nhost = \"db\"\nsslmode = \"verify-ca\"\nsslrootcert = \"/ca.pem\""
        ))
        .unwrap();
        assert_eq!(spec.tls.unwrap().verify, Verify::Ca);
    }

    #[test]
    fn bad_props_are_refused_before_anything_is_dialed() {
        for (props, why) in [
            ("host = \"db\"", "user is missing"),
            ("host = \"db\"\nsocket = \"/run\"\nuser = \"a\"", "not both"),
            (
                "host = \"-oProxyCommand=x\"\nuser = \"a\"",
                "not a host name",
            ),
            ("socket = \"run/pg\"\nuser = \"a\"", "absolute"),
            ("socket = \"/run/../home\"\nuser = \"a\"", "\"..\""),
            ("user = \"a\"\nport = 0", "port must be"),
            ("user = \"a\"\nsslmode = \"require\"", "over a socket"),
            (
                "host = \"db\"\nuser = \"a\"\nsslmode = \"prefer\"",
                "is not disable",
            ),
            ("host = \"db\"\nuser = \"a\\nb\"", "control characters"),
            (
                "host = \"db\"\nuser = \"a\"\nvia = \"web\"",
                "id of an SSH host",
            ),
            (
                "host = \"db\"\nuser = \"a\"\nsslrootcert = \"ca.pem\"",
                "not an absolute path",
            ),
        ] {
            let error = spec(props).unwrap_err();
            assert!(error.contains(why), "{props:?}: {error}");
        }
    }

    #[test]
    fn the_password_target_covers_route_and_tls() {
        let direct = spec("host = \"db\"\nuser = \"app\"").unwrap();
        assert_eq!(
            direct.password_target(None),
            r#""app"@"db:5432"/"app", sslmode="verify-full""#
        );
        let plain = spec("host = \"db\"\nuser = \"app\"\nsslmode = \"disable\"").unwrap();
        assert_ne!(plain.password_target(None), direct.password_target(None));
        let host = node("ssh-host", "alias = \"prod-web\"");
        let through = plain.password_target(Some(&host));
        assert!(
            through.contains(&format!("through \"prod-web\" ({})", host.id)),
            "{through}"
        );
    }

    #[test]
    fn a_name_cant_make_one_password_target_read_as_another() {
        // Unquoted, both would read
        // `app@db:5432/app, sslmode=verify-full, sslrootcert=/a, sslmode=require`.
        let honest =
            spec("host = \"db\"\nuser = \"app\"\nsslrootcert = \"/a, sslmode=require\"").unwrap();
        let forged = spec(
            "host = \"db\"\nuser = \"app\"\nsslmode = \"require\"\n\
             database = \"app, sslmode=verify-full, sslrootcert=/a\"",
        )
        .unwrap();
        assert_ne!(honest.password_target(None), forged.password_target(None));
    }

    #[test]
    fn csv_keeps_null_and_empty_apart() {
        let line = csv_line([Some("a"), None, Some(""), Some("x,\"y\"\n")].into_iter());
        assert_eq!(line, "a,,\"\",\"x,\"\"y\"\"\n\"\r\n");
    }

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
