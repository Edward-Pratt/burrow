//! `burrow`: Burrow on the command line. Like the window, it drives the core
//! in `burrowd`, and starts one when none is running.

mod mcp;
mod tty;

use std::collections::HashMap;
use std::error::Error;
use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use burrow_core::ipc::{self, Client, Events, Role};
use burrow_core::{
    Capability, Command, Event, Grant, GrantShown, Node, NodeId, Response, SSH_TUNNEL_KIND,
    SessionId, SessionKind, SessionState, StatementResult,
};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;

type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

#[derive(Parser)]
#[command(name = "burrow", version, about = "Burrow on the command line")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the nodes in the sidebar.
    Ls {
        /// Print them as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open a terminal on an SSH host.
    Ssh {
        /// The node's name or id.
        node: String,
    },
    /// Run a command on an SSH host and print its output. Exits with the
    /// command's status.
    Exec {
        /// The node's name or id.
        node: String,
        /// Seconds to wait, connecting included.
        #[arg(long, default_value_t = 60)]
        timeout: u32,
        /// The command, run by the remote user's shell as `ssh host cmd`
        /// runs it.
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// Run SQL on a database node and print the rows as tab-separated
    /// values.
    Query {
        /// The node's name or id.
        node: String,
        sql: String,
        /// Let the SQL write. Sessions start read-only.
        #[arg(long)]
        write: bool,
        /// Keep at most this many rows of each statement.
        #[arg(long)]
        limit: Option<u32>,
        /// Print the whole result as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start, stop and list tunnels.
    Tunnel {
        #[command(subcommand)]
        action: TunnelCmd,
    },
    /// Run one API command, given as JSON, and print the response.
    Call {
        /// For example '{"type": "list_sessions"}'. See docs/API.md.
        json: String,
    },
    /// Print events as JSON lines until interrupted.
    Events,
    /// What an AI assistant may do through `burrow mcp serve`.
    Mcp {
        #[command(subcommand)]
        action: McpCmd,
    },
    /// The daemon.
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
}

#[derive(Subcommand)]
enum TunnelCmd {
    /// List tunnels and whether they run.
    Ls,
    /// Start a tunnel and wait until it is up.
    Start { node: String },
    /// Stop a running tunnel.
    Stop { node: String },
}

#[derive(Subcommand)]
enum McpCmd {
    /// Serve MCP on stdin and stdout, for an assistant to start. For Claude
    /// Code: `claude mcp add burrow -- burrow mcp serve`.
    Serve,
    /// List what has been granted.
    Ls,
    /// Let the assistant do more on a node. Shows the node first: the
    /// grant only holds while it stays as shown.
    Grant {
        node: String,
        /// sql-read, sql-write, exec, files-read, files-write, tunnel or monitor-read.
        #[arg(required = true, value_parser = capability)]
        capabilities: Vec<Capability>,
        /// Don't ask.
        #[arg(long, short)]
        yes: bool,
    },
    /// Take some or all of it away.
    Revoke {
        node: String,
        /// What to take away; everything when none are given.
        #[arg(value_parser = capability)]
        capabilities: Vec<Capability>,
    },
}

fn capability(name: &str) -> std::result::Result<Capability, String> {
    Capability::ALL
        .into_iter()
        .find(|c| c.to_string() == name)
        .ok_or_else(|| {
            let names: Vec<String> = Capability::ALL.iter().map(|c| c.to_string()).collect();
            format!("not one of {}", names.join(", "))
        })
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Run the daemon in the foreground, as the systemd unit does.
    Run,
    /// Say whether a daemon is running.
    Status,
    /// Stop the running daemon. Its tunnels stop with it.
    Stop,
}

fn main() -> ExitCode {
    burrow_core::daemon::run_if_asked();
    let cli = Cli::parse();
    if let Cmd::Daemon {
        action: DaemonCmd::Run,
    } = cli.command
    {
        burrow_core::daemon::main();
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => return fail(&e),
    };
    let code = runtime.block_on(run(cli.command));
    // Reading stdin blocks a thread that nothing can wake; don't wait for it.
    runtime.shutdown_timeout(Duration::from_millis(100));
    code.unwrap_or_else(|e| fail(&*e))
}

/// Errors can quote a server, such as an SSH or Postgres message, so their
/// control characters are escaped like any other text from one.
fn fail(error: &dyn Error) -> ExitCode {
    eprintln!("burrow: {}", clean(&error.to_string()));
    ExitCode::FAILURE
}

async fn run(command: Cmd) -> Result<ExitCode> {
    if let Cmd::Daemon { action } = &command {
        return daemon(action).await;
    }
    if let Cmd::Mcp {
        action: McpCmd::Serve,
    } = &command
    {
        return mcp::serve(ipc::connect(Role::Mcp).await?).await;
    }
    let client = ipc::connect(Role::Cli).await?;
    if client.info().build != ipc::BUILD {
        eprintln!(
            "burrow: burrowd {} is still running from before an update; \
             stop what runs in it, then run `burrow daemon stop`",
            clean(&client.info().build)
        );
    }
    match command {
        Cmd::Ls { json } => ls(&client, json).await,
        Cmd::Ssh { node } => ssh(&client, &node).await,
        Cmd::Exec {
            node,
            timeout,
            command,
        } => exec(&client, &node, timeout, command.join(" ")).await,
        Cmd::Query {
            node,
            sql,
            write,
            limit,
            json,
        } => query(&client, &node, sql, write, limit, json).await,
        Cmd::Tunnel { action } => tunnel(&client, action).await,
        Cmd::Call { json } => {
            let command: Command = serde_json::from_str(&json)?;
            let response = client.execute(command).await?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Events => events(&client).await,
        Cmd::Mcp { action } => mcp(&client, action).await,
        Cmd::Daemon { .. } => unreachable!("handled above"),
    }
}

async fn nodes(client: &Client) -> Result<Vec<Node>> {
    match client.execute(Command::ListNodes).await? {
        Response::Nodes { nodes } => Ok(nodes),
        other => Err(unexpected(&other)),
    }
}

/// The node named `arg`, or with `arg` as its id.
async fn find(client: &Client, arg: &str) -> Result<Node> {
    let mut found: Vec<Node> = nodes(client)
        .await?
        .into_iter()
        .filter(|n| n.id.to_string() == arg || n.name == arg)
        .collect();
    match found.len() {
        0 => Err(format!("no node is called {arg:?}").into()),
        1 => Ok(found.remove(0)),
        _ => {
            let ids: Vec<String> = found.iter().map(|n| n.id.to_string()).collect();
            Err(format!(
                "{} nodes are called {arg:?}; name one by id: {}",
                found.len(),
                ids.join(", ")
            )
            .into())
        }
    }
}

async fn ls(client: &Client, json: bool) -> Result<ExitCode> {
    let nodes = nodes(client).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(ExitCode::SUCCESS);
    }
    let mut children: HashMap<Option<NodeId>, Vec<&Node>> = HashMap::new();
    for node in &nodes {
        children.entry(node.parent).or_default().push(node);
    }
    fn print(children: &HashMap<Option<NodeId>, Vec<&Node>>, parent: Option<NodeId>, depth: usize) {
        for node in children.get(&parent).into_iter().flatten() {
            let indent = "  ".repeat(depth);
            println!("{indent}{}\t{}\t{}", clean(&node.name), node.kind, node.id);
            print(children, Some(node.id), depth + 1);
        }
    }
    print(&children, None, 0);
    Ok(ExitCode::SUCCESS)
}

/// Answers, on the terminal, the questions sessions on `node` ask, until
/// the returned task is aborted.
fn answer_prompts(client: &Client, node: NodeId) -> tokio::task::JoinHandle<()> {
    let mut events = client.subscribe();
    let client = client.clone();
    tokio::spawn(async move {
        loop {
            let (session, prompt) = match events.recv().await {
                Ok(Event::PromptRequested { session, prompt }) => (session, prompt),
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            let ours = match client.execute(Command::ListSessions).await {
                Ok(Response::Sessions { sessions }) => sessions
                    .iter()
                    .any(|s| s.session == session && s.node == node),
                _ => false,
            };
            if !ours {
                continue;
            }
            let id = prompt.id;
            let asked = tokio::task::spawn_blocking(move || tty::ask(&prompt));
            if let Ok(Some(answer)) = asked.await {
                // Fails when another client answered first, which is fine.
                let command = Command::AnswerPrompt {
                    session,
                    prompt: id,
                    answer,
                };
                let _ = client.execute(command).await;
            }
        }
    })
}

/// Waits for `session` to connect. Its error when it closes instead.
async fn connected(client: &Client, events: &mut Events, session: SessionId) -> Result<()> {
    loop {
        match events.recv().await {
            Ok(Event::SessionChanged {
                session: s, state, ..
            }) if s == session => match state {
                SessionState::Connected => return Ok(()),
                SessionState::Closed { error } => {
                    return Err(error.unwrap_or_else(|| "it closed".to_string()).into());
                }
                SessionState::Connecting { .. } => {}
            },
            Ok(_) => {}
            // A missed event may have been ours: ask how the session is.
            Err(RecvError::Lagged(_)) => {
                let sessions = match client.execute(Command::ListSessions).await? {
                    Response::Sessions { sessions } => sessions,
                    other => return Err(unexpected(&other)),
                };
                match sessions.iter().find(|s| s.session == session) {
                    Some(info) if info.state == SessionState::Connected => return Ok(()),
                    Some(_) => {}
                    None => return Err("it closed".into()),
                }
            }
            Err(RecvError::Closed) => return Err("lost the connection to burrowd".into()),
        }
    }
}

async fn session(client: &Client, command: Command) -> Result<SessionId> {
    match client.execute(command).await? {
        Response::Session { session } => Ok(session),
        other => Err(unexpected(&other)),
    }
}

async fn ssh(client: &Client, node: &str) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let (cols, rows) = tty::size();
    let mut events = client.subscribe();
    let prompts = answer_prompts(client, node.id);
    let command = Command::OpenTerminal {
        node: node.id,
        cols,
        rows,
    };
    let session = session(client, command).await?;
    let mut output = client.take_output(session).await?;
    let ready = connected(client, &mut events, session).await;
    prompts.abort();
    ready?;

    let raw = tty::Mode::raw();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut resized =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut typed = [0u8; 4096];
    loop {
        tokio::select! {
            data = output.recv() => match data {
                Some(data) => {
                    stdout.write_all(&data).await?;
                    stdout.flush().await?;
                }
                None => break,
            },
            read = stdin.read(&mut typed) => match read {
                Ok(0) | Err(_) => {
                    let _ = client.execute(Command::CloseSession { session }).await;
                    break;
                }
                Ok(n) => {
                    let data = typed[..n].to_vec();
                    client.execute(Command::WriteTerminal { session, data }).await?;
                }
            },
            _ = resized.recv() => {
                let (cols, rows) = tty::size();
                let _ = client.execute(Command::ResizeTerminal { session, cols, rows }).await;
            }
        }
    }
    drop(raw);
    // The reason it ended, when it was an error.
    let ended = tokio::time::timeout(
        Duration::from_secs(1),
        connected(client, &mut events, session),
    )
    .await;
    match ended {
        Ok(Err(error)) if error.to_string() != "it closed" => Err(error),
        _ => Ok(ExitCode::SUCCESS),
    }
}

async fn exec(client: &Client, node: &str, timeout: u32, command: String) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let prompts = answer_prompts(client, node.id);
    let command = Command::Exec {
        node: node.id,
        command,
        timeout: Some(timeout),
    };
    let result = client.execute(command).await;
    prompts.abort();
    let result = match result? {
        Response::Exec { result } => result,
        other => return Err(unexpected(&other)),
    };
    // The command's own output, passed on as `ssh host cmd` would.
    std::io::stdout().write_all(result.stdout.as_bytes())?;
    std::io::stderr().write_all(result.stderr.as_bytes())?;
    if result.truncated {
        eprintln!("burrow: output was cut at 1 MiB");
    }
    if result.timed_out {
        eprintln!("burrow: the command was still running after {timeout} s");
        // What timeout(1) exits with.
        return Ok(ExitCode::from(124));
    }
    Ok(match (result.status, result.signal) {
        (Some(status), _) => ExitCode::from(u8::try_from(status).unwrap_or(255)),
        (None, Some(signal)) => {
            eprintln!("burrow: the command was ended by {}", clean(&signal));
            ExitCode::from(255)
        }
        (None, None) => ExitCode::from(255),
    })
}

async fn query(
    client: &Client,
    node: &str,
    sql: String,
    write: bool,
    limit: Option<u32>,
    json: bool,
) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let mut events = client.subscribe();
    let prompts = answer_prompts(client, node.id);
    let session = session(client, Command::OpenDatabase { node: node.id }).await?;
    let ready = connected(client, &mut events, session).await;
    prompts.abort();
    ready?;
    let result = async {
        if write {
            let command = Command::SetReadOnly {
                session,
                read_only: false,
            };
            client.execute(command).await?;
        }
        let command = Command::RunQuery {
            session,
            sql,
            limit,
        };
        client.execute(command).await
    }
    .await;
    let _ = client.execute(Command::CloseSession { session }).await;
    let result = match result? {
        Response::Query { result } => result,
        other => return Err(unexpected(&other)),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let mut out = std::io::stdout().lock();
        for statement in &result.statements {
            print_statement(&mut out, statement)?;
        }
    }
    if result.stopped {
        eprintln!("burrow: stopped at the row limit; use --limit for more");
    }
    match result.error {
        Some(error) => {
            eprintln!("ERROR: {}", clean(&error.message));
            for (label, text) in [("DETAIL", &error.detail), ("HINT", &error.hint)] {
                if let Some(text) = text {
                    eprintln!("{label}: {}", clean(text));
                }
            }
            Ok(ExitCode::FAILURE)
        }
        None => Ok(ExitCode::SUCCESS),
    }
}

/// Rows as tab-separated values, as `COPY … TO STDOUT` writes them:
/// NULL is `\N`, and tabs, newlines and backslashes in values are escaped.
fn print_statement(out: &mut impl Write, statement: &StatementResult) -> Result<()> {
    if statement.columns.is_empty() {
        eprintln!("{} rows", statement.count);
        return Ok(());
    }
    let header: Vec<String> = statement.columns.iter().map(|c| field(Some(c))).collect();
    writeln!(out, "{}", header.join("\t"))?;
    for row in &statement.rows {
        let row: Vec<String> = row.iter().map(|v| field(v.as_deref())).collect();
        writeln!(out, "{}", row.join("\t"))?;
    }
    if statement.truncated {
        eprintln!(
            "burrow: only the first {} rows are shown",
            statement.rows.len()
        );
    }
    Ok(())
}

fn field(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "\\N".to_string();
    };
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

/// `text` with control characters escaped, for names and messages that
/// came from a server or a config file someone else may have written.
fn clean(text: &str) -> String {
    text.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_control() {
                Box::new(c.escape_unicode())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

async fn tunnel(client: &Client, action: TunnelCmd) -> Result<ExitCode> {
    match action {
        TunnelCmd::Ls => {
            let sessions = match client.execute(Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                other => return Err(unexpected(&other)),
            };
            for node in nodes(client).await? {
                if node.kind != SSH_TUNNEL_KIND {
                    continue;
                }
                let running = sessions
                    .iter()
                    .find(|s| s.node == node.id && s.kind == SessionKind::Tunnel);
                let state = match running.map(|s| &s.state) {
                    Some(SessionState::Connected) => "up".to_string(),
                    Some(SessionState::Connecting { step }) => {
                        format!("connecting: {}", clean(step))
                    }
                    Some(SessionState::Closed { .. }) | None => "stopped".to_string(),
                };
                println!("{}\t{state}\t{}", clean(&node.name), node.id);
            }
            Ok(ExitCode::SUCCESS)
        }
        TunnelCmd::Start { node } => {
            let node = find(client, &node).await?;
            let mut events = client.subscribe();
            let prompts = answer_prompts(client, node.id);
            let session = session(client, Command::StartTunnel { node: node.id }).await?;
            // Already up: starting it again returned the running session.
            let up = matches!(
                client.execute(Command::ListSessions).await?,
                Response::Sessions { sessions } if sessions.iter().any(|s| s.session == session && s.state == SessionState::Connected)
            );
            let ready = if up {
                Ok(())
            } else {
                connected(client, &mut events, session).await
            };
            prompts.abort();
            ready?;
            eprintln!("{} is up", clean(&node.name));
            Ok(ExitCode::SUCCESS)
        }
        TunnelCmd::Stop { node } => {
            let node = find(client, &node).await?;
            let sessions = match client.execute(Command::ListSessions).await? {
                Response::Sessions { sessions } => sessions,
                other => return Err(unexpected(&other)),
            };
            let Some(running) = sessions
                .iter()
                .find(|s| s.node == node.id && s.kind == SessionKind::Tunnel)
            else {
                return Err(format!("{} is not running", clean(&node.name)).into());
            };
            let command = Command::CloseSession {
                session: running.session,
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn grants(client: &Client) -> Result<Vec<Grant>> {
    match client.execute(Command::ListGrants).await? {
        Response::Grants { grants } => Ok(grants),
        other => Err(unexpected(&other)),
    }
}

async fn mcp(client: &Client, action: McpCmd) -> Result<ExitCode> {
    match action {
        McpCmd::Serve => unreachable!("handled in run"),
        McpCmd::Ls => {
            let nodes = nodes(client).await?;
            for grant in grants(client).await? {
                let name = nodes
                    .iter()
                    .find(|n| n.id == grant.node)
                    .map(|n| clean(&n.name))
                    .unwrap_or_default();
                let capabilities: Vec<String> =
                    grant.capabilities.iter().map(|c| c.to_string()).collect();
                let stale = if grant.stale {
                    "\tstale: changed since granted"
                } else {
                    ""
                };
                println!("{name}\t{}\t{}{stale}", capabilities.join(","), grant.node);
            }
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Grant {
            node,
            capabilities,
            yes,
        } => {
            let nodes = nodes(client).await?;
            let node = find(client, &node).await?;
            let shown = GrantShown::of(&node, &nodes);
            eprint!("{}", describe(&node.kind, &shown));
            let mut all: std::collections::BTreeSet<Capability> = grants(client)
                .await?
                .into_iter()
                .find(|g| g.node == node.id && !g.stale)
                .map(|g| g.capabilities)
                .unwrap_or_default();
            all.extend(capabilities);
            let list: Vec<String> = all.iter().map(|c| c.to_string()).collect();
            if !yes {
                if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                    return Err("not asking without a terminal; pass --yes".into());
                }
                eprint!("Let the assistant use {} here? [y/N] ", list.join(", "));
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer)?;
                if !matches!(answer.trim(), "y" | "Y" | "yes") {
                    return Ok(ExitCode::FAILURE);
                }
            }
            let command = Command::SetGrant {
                node: node.id,
                capabilities: all,
                expect: Some(shown),
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
        McpCmd::Revoke { node, capabilities } => {
            let node = find(client, &node).await?;
            let grant = grants(client)
                .await?
                .into_iter()
                .find(|g| g.node == node.id);
            let Some(grant) = grant else {
                return Ok(ExitCode::SUCCESS);
            };
            let mut left = grant.capabilities;
            left.retain(|c| !capabilities.is_empty() && !capabilities.contains(c));
            // What is left gets pinned again, to the node as it is now. A
            // stale grant changed after the user saw it, so it isn't kept.
            let expect = match left.is_empty() {
                true => None,
                false if grant.stale => {
                    return Err(format!(
                        "{} changed since it was granted; revoke it all, then grant it again",
                        clean(&node.name)
                    )
                    .into());
                }
                false => Some(GrantShown::of(&node, &nodes(client).await?)),
            };
            let command = Command::SetGrant {
                node: node.id,
                capabilities: left,
                expect,
            };
            client.execute(command).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// A node's settings, as shown before granting, with anything that came
/// from a file escaped.
fn describe(kind: &str, shown: &GrantShown) -> String {
    let mut text = format!("{} ({kind})\n", clean(&shown.name));
    for (key, value) in &shown.props {
        text.push_str(&format!(
            "  {} = {}\n",
            clean(key),
            clean(&value.to_string())
        ));
    }
    if let Some(via) = &shown.via {
        text.push_str(&format!("  through {}\n", clean(&via.name)));
        for (key, value) in &via.props {
            text.push_str(&format!(
                "    {} = {}\n",
                clean(key),
                clean(&value.to_string())
            ));
        }
    }
    text
}

async fn events(client: &Client) -> Result<ExitCode> {
    let mut events = client.subscribe();
    loop {
        match events.recv().await {
            Ok(event) => println!("{}", serde_json::to_string(&event)?),
            Err(RecvError::Lagged(_)) => eprintln!("burrow: missed some events"),
            Err(RecvError::Closed) => return Err("lost the connection to burrowd".into()),
        }
    }
}

async fn daemon(action: &DaemonCmd) -> Result<ExitCode> {
    let dir = ipc::runtime_dir()?;
    let socket = dir.join("burrowd.sock");
    match action {
        DaemonCmd::Run => unreachable!("handled in main"),
        DaemonCmd::Status => match Client::connect(&socket, Role::Cli).await {
            Ok(client) => {
                let sessions = match client.execute(Command::ListSessions).await? {
                    Response::Sessions { sessions } => sessions.len(),
                    _ => 0,
                };
                println!(
                    "burrowd {} is running in {} with {sessions} sessions",
                    client.info().build,
                    dir.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Err(_) => {
                println!("burrowd is not running in {}", dir.display());
                Ok(ExitCode::from(3))
            }
        },
        DaemonCmd::Stop => {
            if !ipc::stop(&dir).await? {
                println!("burrowd is not running in {}", dir.display());
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn unexpected(response: &Response) -> Box<dyn Error> {
    format!("burrowd sent an unexpected reply: {response:?}").into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_print_as_copy_does() {
        assert_eq!(field(None), "\\N");
        assert_eq!(field(Some("a\tb\nc\\d")), "a\\tb\\nc\\\\d");
        assert_eq!(field(Some("\u{1b}[31m")), "\\u{1b}[31m");
    }

    #[test]
    fn what_a_grant_shows_is_escaped_keys_too() {
        let mut props = toml::Table::new();
        props.insert("alias\u{1b}[2K".into(), "web\u{1b}[2K".into());
        let shown = GrantShown {
            name: "web".into(),
            props,
            via: None,
        };
        let text = describe("ssh-host", &shown);
        assert!(!text.contains('\u{1b}'), "{text:?}");
    }

    #[test]
    fn names_lose_their_control_characters() {
        assert_eq!(clean("web\u{1b}]0;owned\u{7}"), "web\\u{1b}]0;owned\\u{7}");
        assert_eq!(clean("plain"), "plain");
    }
}
