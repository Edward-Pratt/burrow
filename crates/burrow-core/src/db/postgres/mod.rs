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
//!
//! This file connects and signs in. `spec` reads the node, `query` runs
//! SQL, `schema` lists the database, `export` writes CSV and `tls` is the
//! rustls connector.

use tokio::task::JoinHandle;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::tls::{NoTls, TlsConnect};
use tokio_postgres::{CancelToken, Config};

use crate::api::{Node, PromptAnswer, PromptKind};
use crate::engine::Core;
use crate::net::{self, Link, Route};
use crate::password::{Saved, save_password, saved_password};
use crate::ssh::Prompter;

mod export;
mod query;
mod schema;
mod spec;
mod tls;

pub(crate) use spec::Spec;
use tls::Rustls;

type Result<T> = std::result::Result<T, String>;

/// Wrong passwords allowed before giving up, as for SSH.
const ATTEMPTS: usize = 3;

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
