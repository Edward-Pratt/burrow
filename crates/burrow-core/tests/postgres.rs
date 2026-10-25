//! Database sessions against a real PostgreSQL started for each test, in a
//! throwaway data folder, on a free port of 127.0.0.1 and a socket in its
//! own folder. The superuser is the user running the tests, so peer
//! sign-in over the socket works, and its password is "secret" over TCP.
//!
//! Skipped (with a note) when PostgreSQL isn't installed, or when running
//! as root, which `initdb` refuses.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Process, Stdio};
use std::time::Duration;

use burrow_core::{
    Command, Core, DbTableKind, Event, Node, NodePatch, PromptAnswer, PromptKind, QueryResult,
    Response, SecretValue, SessionId, SessionState,
};
use tempfile::TempDir;
use tokio::sync::broadcast;

mod common;
use common::*;

const INITDB: &str = "/usr/bin/initdb";
const POSTGRES: &str = "/usr/bin/postgres";
const PASSWORD: &str = "secret";

struct Postgres {
    /// Short, under /tmp: a socket path must fit in 107 bytes.
    dir: TempDir,
    port: u16,
    server: Child,
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

/// Server certificates, signed by a CA of their own.
struct Tls {
    ca: PathBuf,
}

impl Postgres {
    fn start() -> Option<Postgres> {
        Postgres::start_with(false).map(|(postgres, _)| postgres)
    }

    /// With `tls`, the server also takes TLS, with a certificate for
    /// "localhost".
    fn start_with(tls: bool) -> Option<(Postgres, Option<Tls>)> {
        if !Path::new(INITDB).exists() {
            eprintln!("skipping: {INITDB} is not installed");
            return None;
        }
        if whoami() == "root" {
            eprintln!("skipping: initdb won't run as root");
            return None;
        }
        let dir = tempfile::Builder::new()
            .prefix("bpg")
            .tempdir_in("/tmp")
            .unwrap();
        let data = dir.path().join("data");
        let pwfile = dir.path().join("pw");
        fs::write(&pwfile, PASSWORD).unwrap();
        let status = Process::new(INITDB)
            .arg("-D")
            .arg(&data)
            .args(["-U", &whoami(), "-E", "UTF8", "--no-sync"])
            .args(["--auth-local=peer", "--auth-host=scram-sha-256"])
            .arg(format!("--pwfile={}", pwfile.display()))
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "initdb failed");

        let port = free_port();
        let mut server = Process::new(POSTGRES);
        server
            .arg("-D")
            .arg(&data)
            .args(["-p", &port.to_string(), "-k"])
            .arg(dir.path())
            .args(["-c", "listen_addresses=127.0.0.1", "-c", "fsync=off"])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let tls = tls.then(|| {
            let (ca, cert, key) = certs("localhost");
            let path = |name: &str| dir.path().join(name);
            fs::write(path("ca.pem"), ca).unwrap();
            fs::write(path("server.crt"), cert).unwrap();
            fs::write(path("server.key"), key).unwrap();
            // Postgres refuses a key anyone else can read.
            fs::set_permissions(path("server.key"), fs::Permissions::from_mode(0o600)).unwrap();
            server
                .args(["-c", "ssl=on", "-c"])
                .arg(format!("ssl_cert_file={}", path("server.crt").display()))
                .arg("-c")
                .arg(format!("ssl_key_file={}", path("server.key").display()));
            Tls { ca: path("ca.pem") }
        });
        let postgres = Postgres {
            server: server.spawn().unwrap(),
            dir,
            port,
        };
        postgres.wait_until_ready();
        Some((postgres, tls))
    }

    fn wait_until_ready(&self) {
        for _ in 0..200 {
            let ready = Process::new("pg_isready")
                .args(["-q", "-h", "127.0.0.1", "-p", &self.port.to_string()])
                .status()
                .is_ok_and(|s| s.success());
            if ready {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("postgres did not start on port {}", self.port);
    }

    fn socket_dir(&self) -> String {
        self.dir.path().display().to_string()
    }

    /// Props for reaching it over TCP as the superuser, without TLS.
    fn tcp(&self) -> String {
        format!(
            "host = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\ndatabase = \"postgres\"\n\
             sslmode = \"disable\"\n",
            self.port,
            whoami()
        )
    }
}

/// A CA, and a server certificate and key it signed for `name`, as PEM.
fn certs(name: &str) -> (String, String, String) {
    use rcgen::{BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair};
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec![name.to_string()])
        .unwrap()
        .signed_by(&key, &ca)
        .unwrap();
    (ca.pem(), cert.pem(), key.serialize_pem())
}

/// A core of its own, in a folder of its own.
fn core() -> (TempDir, Core) {
    let dir = TempDir::new().unwrap();
    let core = Core::open(burrow_core::CoreOptions {
        config_dir: dir.path().join("burrow"),
        state_dir: dir.path().join("state"),
        secrets: std::sync::Arc::new(burrow_core::MemoryStore::new()),
        ssh_config: None,
    })
    .unwrap();
    (dir, core)
}

async fn database(core: &Core, props: &str) -> Node {
    let command = Command::CreateNode {
        parent: None,
        kind: "postgres".into(),
        name: "db".into(),
        color: None,
        props: toml::from_str(props).unwrap(),
    };
    match core.execute(command).await.unwrap() {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    }
}

async fn open_database(core: &Core, node: &Node) -> SessionId {
    match core
        .execute(Command::OpenDatabase { node: node.id })
        .await
        .unwrap()
    {
        Response::Session { session } => session,
        other => panic!("expected a session, got {other:?}"),
    }
}

/// Opens a session, answering a password prompt with `password` if one
/// comes, and waits until it is connected.
async fn connect(core: &Core, node: &Node, password: Option<&str>) -> SessionId {
    let mut events = core.subscribe();
    let session = open_database(core, node).await;
    loop {
        match next(&mut events, session).await {
            Next::Prompt(prompt) => {
                let value = password.unwrap_or_else(|| panic!("unexpected prompt {prompt:?}"));
                answer(core, session, &prompt, text(value, true)).await;
            }
            Next::State(SessionState::Connected) => return session,
            Next::State(state) => panic!("expected connected, got {state:?}"),
        }
    }
}

fn text(value: &str, save: bool) -> PromptAnswer {
    PromptAnswer::Text {
        value: SecretValue::new(value),
        save,
    }
}

async fn query(core: &Core, session: SessionId, sql: &str, limit: Option<u32>) -> QueryResult {
    let command = Command::RunQuery {
        session,
        sql: sql.into(),
        limit,
    };
    match core.execute(command).await.unwrap() {
        Response::Query { result } => result,
        other => panic!("expected a result, got {other:?}"),
    }
}

/// The values of a query's only statement.
async fn values(core: &Core, session: SessionId, sql: &str) -> Vec<Vec<Option<String>>> {
    let result = query(core, session, sql, None).await;
    assert_eq!(result.error, None, "{sql}");
    assert_eq!(result.statements.len(), 1, "{sql}");
    result.statements.into_iter().next().unwrap().rows
}

async fn writable(core: &Core, session: SessionId) {
    let command = Command::SetReadOnly {
        session,
        read_only: false,
    };
    core.execute(command).await.unwrap();
}

fn one(value: &str) -> Vec<Vec<Option<String>>> {
    vec![vec![Some(value.to_string())]]
}

#[tokio::test(flavor = "multi_thread")]
async fn signs_in_over_tcp_and_saves_the_password_for_that_target_only() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;

    let mut events = core.subscribe();
    let session = open_database(&core, &node).await;
    let prompt = expect_prompt(&mut events, session).await;
    assert_eq!(prompt.kind, PromptKind::Secret { can_save: true });
    assert!(
        prompt.message.contains(r#"sslmode="disable""#),
        "{}",
        prompt.message
    );
    answer(&core, session, &prompt, text("wrong", false)).await;
    let prompt = expect_prompt(&mut events, session).await;
    assert!(
        prompt.message.starts_with("That didn't work"),
        "{}",
        prompt.message
    );
    answer(&core, session, &prompt, text(PASSWORD, true)).await;
    assert_eq!(
        expect_state(&mut events, session).await,
        SessionState::Connected
    );
    assert_eq!(values(&core, session, "select 1 as one").await, one("1"));
    core.execute(Command::CloseSession { session })
        .await
        .unwrap();

    // Saved, so the next session asks nothing.
    let session = connect(&core, &node, None).await;
    core.execute(Command::CloseSession { session })
        .await
        .unwrap();

    // Sent through another host, or without TLS where it had it, the
    // password could reach someone else: Burrow asks instead of sending it.
    let mut props = node.props.clone();
    props.insert("database".into(), "template1".into());
    let patch = NodePatch {
        props: Some(props),
        ..NodePatch::default()
    };
    core.execute(Command::UpdateNode { id: node.id, patch })
        .await
        .unwrap();
    let session = open_database(&core, &node).await;
    let prompt = expect_prompt(&mut events, session).await;
    assert!(
        prompt.message.contains("The saved password is for"),
        "{}",
        prompt.message
    );
    answer(&core, session, &prompt, PromptAnswer::Cancel).await;
    let state = expect_state(&mut events, session).await;
    assert!(
        closed_error(state).unwrap().contains("cancelled"),
        "a cancelled prompt ends the session"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn peer_sign_in_over_the_socket_asks_nothing() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let props = format!(
        "socket = \"{}\"\nport = {}\nuser = \"{}\"\ndatabase = \"postgres\"",
        postgres.socket_dir(),
        postgres.port,
        whoami()
    );
    let node = database(&core, &props).await;
    let session = connect(&core, &node, None).await;
    assert_eq!(
        values(&core, session, "select current_user").await,
        one(&whoami())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_start_read_only_until_told_otherwise() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;

    let result = query(&core, session, "create table t (n int)", None).await;
    assert_eq!(result.error.unwrap().code.as_deref(), Some("25006"));

    writable(&core, session).await;
    let result = query(
        &core,
        session,
        "create table t (n int); insert into t values (1), (2)",
        None,
    )
    .await;
    assert_eq!(result.error, None);
    let counts: Vec<u64> = result.statements.iter().map(|s| s.count).collect();
    assert_eq!(counts, [0, 2]);
    assert_eq!(
        values(&core, session, "select count(*) from t").await,
        one("2")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reports_each_statement_and_where_an_error_is() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;

    let result = query(
        &core,
        session,
        "select 1 as a, null as b, '' as c; select 'x\ny' as d",
        None,
    )
    .await;
    assert_eq!(result.error, None);
    assert_eq!(result.statements.len(), 2);
    assert_eq!(result.statements[0].columns, ["a", "b", "c"]);
    assert_eq!(
        result.statements[0].rows,
        [[Some("1".to_string()), None, Some(String::new())]]
    );
    assert_eq!(result.statements[1].rows, one("x\ny"));

    let result = query(&core, session, "select 1; select * from nope", None).await;
    assert_eq!(result.statements.len(), 1, "the first statement still ran");
    let error = result.error.unwrap();
    assert_eq!(error.code.as_deref(), Some("42P01"));
    assert_eq!(error.position, Some(25));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_query_with_too_many_rows_is_stopped_at_the_limit() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;

    let sql = "select n from generate_series(1, 100000000) as n";
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        query(&core, session, sql, Some(10)),
    )
    .await
    .expect("the query was not stopped");
    assert!(result.stopped, "{result:?}");
    assert_eq!(result.error, None);
    let statement = &result.statements[0];
    assert!(statement.truncated);
    assert_eq!(statement.rows.len(), 10);
    assert_eq!(statement.rows[9], [Some("10".to_string())]);

    // The session is fine afterwards, and the late cancel hit nothing.
    assert_eq!(values(&core, session, "select 2").await, one("2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_running_query_can_be_cancelled() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;

    let running = tokio::spawn({
        let core = core.clone();
        async move { query(&core, session, "select pg_sleep(30)", None).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    core.execute(Command::CancelQuery { session })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the query was not cancelled")
        .unwrap();
    assert_eq!(result.error.unwrap().code.as_deref(), Some("57014"));
}

#[tokio::test(flavor = "multi_thread")]
async fn lists_schemas_tables_and_columns() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    writable(&core, session).await;
    let result = query(
        &core,
        session,
        "create schema shop; \
         create table shop.items (id serial primary key, name varchar(40)); \
         create view shop.names as select name from shop.items; \
         create table shop.empty ()",
        None,
    )
    .await;
    assert_eq!(result.error, None);

    let schemas = match core.execute(Command::ListSchema { session }).await.unwrap() {
        Response::Schema { schemas } => schemas,
        other => panic!("expected a schema, got {other:?}"),
    };
    let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["public", "shop"]);
    let shop = &schemas[1];
    let tables: Vec<(&str, DbTableKind)> = shop
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t.kind))
        .collect();
    assert_eq!(
        tables,
        [
            ("empty", DbTableKind::Table),
            ("items", DbTableKind::Table),
            ("names", DbTableKind::View)
        ]
    );
    assert!(shop.tables[0].columns.is_empty());
    let items = &shop.tables[1].columns;
    assert_eq!(items[0].name, "id");
    assert!(!items[0].nullable);
    assert_eq!(items[1].data_type, "character varying(40)");
    assert!(items[1].nullable);
}

#[tokio::test(flavor = "multi_thread")]
async fn exports_rows_as_csv_but_not_inside_an_open_transaction() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    let path = dir.path().join("out.csv");
    let export = |sql: &str| Command::ExportQuery {
        session,
        sql: sql.into(),
        path: path.clone(),
    };

    let sql = "select n, case when n = 2 then null else 'a,' || n end as t \
               from generate_series(1, 3) as n";
    match core.execute(export(sql)).await.unwrap() {
        Response::Exported { rows } => assert_eq!(rows, 3),
        other => panic!("expected an export, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "n,t\r\n1,\"a,1\"\r\n2,\r\n3,\"a,3\"\r\n"
    );

    // Rolling back at the end would throw away the user's open work.
    assert_eq!(query(&core, session, "begin", None).await.error, None);
    let error = core.execute(export("select 1")).await.unwrap_err();
    assert!(error.to_string().contains("open transaction"), "{error}");
    assert_eq!(query(&core, session, "commit", None).await.error, None);

    // Exports are read-only, even from a writable session.
    writable(&core, session).await;
    let error = core
        .execute(export("create table t (n int)"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("read-only"), "{error}");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with(".burrow-export"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn keeps_a_history_that_goes_with_the_node() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    query(&core, session, "select 1", None).await;
    query(&core, session, "select nope", None).await;

    let history = |node: &Node| Command::ListHistory { node: node.id };
    let entries = match core.execute(history(&node)).await.unwrap() {
        Response::History { entries } => entries,
        other => panic!("expected a history, got {other:?}"),
    };
    let seen: Vec<(&str, bool)> = entries.iter().map(|e| (e.sql.as_str(), e.ok)).collect();
    assert_eq!(seen, [("select 1", true), ("select nope", false)]);

    let command = Command::DeleteNode {
        id: node.id,
        recursive: false,
    };
    core.execute(command).await.unwrap();
    let node = database(&core, &postgres.tcp()).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    query(&core, session, "select 2", None).await;
    let command = Command::ClearHistory { node: node.id };
    core.execute(command).await.unwrap();
    match core.execute(history(&node)).await.unwrap() {
        Response::History { entries } => assert!(entries.is_empty()),
        other => panic!("expected a history, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn verify_full_checks_the_certificate_against_the_root_given() {
    let Some((postgres, Some(tls))) = Postgres::start_with(true) else {
        return;
    };
    let (_dir, core) = core();
    let props = |root: &str| {
        format!(
            "host = \"localhost\"\nport = {}\nuser = \"{}\"\ndatabase = \"postgres\"\n{root}",
            postgres.port,
            whoami()
        )
    };
    let node = database(
        &core,
        &props(&format!("sslrootcert = \"{}\"", tls.ca.display())),
    )
    .await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    assert_eq!(
        values(
            &core,
            session,
            "select ssl from pg_stat_ssl where pid = pg_backend_pid()"
        )
        .await,
        one("t")
    );

    // The system's CAs never signed this server's certificate.
    let node = database(&core, &props("")).await;
    let mut events = core.subscribe();
    let session = open_database(&core, &node).await;
    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("certificate"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_tls_says_how_to_reach_it() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (_dir, core) = core();
    // Over the network, TLS is on unless the props turn it off.
    let props = postgres.tcp().replace("sslmode = \"disable\"\n", "");
    let node = database(&core, &props).await;
    let mut events = core.subscribe();
    let session = open_database(&core, &node).await;
    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("Turn TLS off"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reaches_the_server_through_an_ssh_host_by_port_and_by_socket() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let Some(server) = Server::start() else {
        return;
    };
    trust(&server);
    let core = server.core();
    let host = host(&core, "test").await;

    let via = format!("via = \"{}\"\n", host.id);
    let node = database(&core, &format!("{via}{}", postgres.tcp())).await;
    let session = connect(&core, &node, Some(PASSWORD)).await;
    assert_eq!(values(&core, session, "select 1").await, one("1"));

    // A Unix socket on the far side signs in as the SSH user, by peer.
    let props = format!(
        "{via}socket = \"{}\"\nport = {}\nuser = \"{}\"\ndatabase = \"postgres\"",
        postgres.socket_dir(),
        postgres.port,
        whoami()
    );
    let node = database(&core, &props).await;
    let session = connect(&core, &node, None).await;
    assert_eq!(values(&core, session, "select 2").await, one("2"));

    // Cancelling dials the same route again.
    let running = tokio::spawn({
        let core = core.clone();
        async move { query(&core, session, "select pg_sleep(30)", None).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    core.execute(Command::CancelQuery { session })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the query was not cancelled")
        .unwrap();
    assert_eq!(result.error.unwrap().code.as_deref(), Some("57014"));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_database_nodes_open_and_only_database_sessions_query() {
    let (_dir, core) = core();
    let folder = Command::CreateNode {
        parent: None,
        kind: "folder".into(),
        name: "f".into(),
        color: None,
        props: Default::default(),
    };
    let Response::Node { node } = core.execute(folder).await.unwrap() else {
        panic!("expected a node");
    };
    let error = core
        .execute(Command::OpenDatabase { node: node.id })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a database"), "{error}");
    let error = core
        .execute(Command::ListHistory { node: node.id })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not a database"), "{error}");

    // Bad props fail the session before anything is dialed.
    let node = database(&core, "host = \"db\"").await;
    let mut events: broadcast::Receiver<Event> = core.subscribe();
    let session = open_database(&core, &node).await;
    let error = closed_error(expect_state(&mut events, session).await).unwrap();
    assert!(error.contains("user is missing"), "{error}");
    let error = query_error(&core, session).await;
    assert!(error.contains("not found"), "{error}");
}

async fn query_error(core: &Core, session: SessionId) -> String {
    let command = Command::RunQuery {
        session,
        sql: "select 1".into(),
        limit: None,
    };
    core.execute(command).await.unwrap_err().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_mcp_bridge_cannot_write_without_a_write_grant() {
    use burrow_core::Capability::{SqlRead, SqlWrite};
    use burrow_core::ipc::{Client, Role};

    let Some(postgres) = Postgres::start() else {
        return;
    };
    let (dir, core) = core();
    // The test user is a superuser: it makes a role that can only read.
    let admin = format!(
        "socket = \"{}\"\nport = {}\nuser = \"{}\"\ndatabase = \"postgres\"",
        postgres.socket_dir(),
        postgres.port,
        whoami()
    );
    let admin = database(&core, &admin).await;
    let session = connect(&core, &admin, None).await;
    writable(&core, session).await;
    let setup = "create role reader login password 'secret'; \
                 grant pg_read_all_data to reader; grant create on schema public to reader";
    assert_eq!(query(&core, session, setup, None).await.error, None);
    let props = postgres.tcp().replace(&whoami(), "reader");
    let reader = database(&core, &props).await;
    // Its password is saved, so the bridge's sessions ask nothing.
    let session = connect(&core, &reader, Some(PASSWORD)).await;
    core.execute(Command::CloseSession { session })
        .await
        .unwrap();

    let socket = serve(core, dir.path());
    let gui = Client::connect(&socket, Role::Gui).await.unwrap();
    let mcp = Client::connect(&socket, Role::Mcp).await.unwrap();
    let open = |node: &Node| Command::OpenDatabase { node: node.id };
    assert!(mcp.execute(open(&reader)).await.is_err());
    let run = async |session, sql: &str| match mcp
        .execute(Command::RunQuery {
            session,
            sql: sql.into(),
            limit: None,
        })
        .await
        .unwrap()
    {
        Response::Query { result } => result,
        other => panic!("expected a result, got {other:?}"),
    };

    // A superuser can write files and run programs from a read-only
    // transaction, so it gets nothing read-only.
    grant(&gui, &admin, &[SqlRead]).await;
    let session = open_and_wait(&mcp, open(&admin)).await;
    let refused = run(session, "select 1").await.error.unwrap();
    assert!(refused.message.contains("can't keep this role read-only"));

    grant(&gui, &reader, &[SqlRead]).await;
    let session = open_and_wait(&mcp, open(&reader)).await;
    assert_eq!(run(session, "select 1").await.error, None);
    // Each way out of read-only: plainly, by changing the setting, and by
    // ending the transaction. The last two are refused as two statements.
    for sql in [
        "create table t (n int)",
        "set default_transaction_read_only = off; create table t (n int)",
        "commit; create table t (n int)",
    ] {
        assert!(run(session, sql).await.error.is_some(), "{sql} ran");
    }
    let error = mcp
        .execute(Command::SetReadOnly {
            session,
            read_only: false,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("sql-write"), "{error}");
    let missing = run(session, "select to_regclass('t') is null").await;
    assert_eq!(missing.statements[0].rows, one("t"));

    // With a write grant it may make the session writable.
    grant(&gui, &reader, &[SqlWrite]).await;
    mcp.execute(Command::SetReadOnly {
        session,
        read_only: false,
    })
    .await
    .unwrap();
    assert_eq!(run(session, "create table t (n int)").await.error, None);
}
