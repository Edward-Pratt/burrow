//! End-to-end tests of the command/event API against a temporary config
//! directory and an in-memory secret store. Nothing here touches
//! `~/.config/burrow` or the real keyring.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use burrow_core::{
    Command, Core, CoreOptions, Error, Event, MemoryStore, Node, NodeId, NodePatch, Response,
    SecretValue,
};
use tempfile::TempDir;
use tokio::sync::broadcast;

struct Fixture {
    dir: TempDir,
    core: Core,
    secrets: Arc<MemoryStore>,
}

impl Fixture {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let secrets = Arc::new(MemoryStore::new());
        let core = open(dir.path(), secrets.clone());
        Fixture { dir, core, secrets }
    }

    fn tree(&self) -> std::path::PathBuf {
        self.dir.path().join("tree")
    }

    async fn run(&self, command: Command) -> Response {
        self.core.execute(command).await.unwrap()
    }

    async fn create(&self, parent: Option<NodeId>, kind: &str, name: &str) -> Node {
        node(
            self.run(Command::CreateNode {
                parent,
                kind: kind.into(),
                name: name.into(),
                color: None,
                props: toml::Table::new(),
            })
            .await,
        )
    }

    async fn nodes(&self) -> Vec<Node> {
        match self.run(Command::ListNodes).await {
            Response::Nodes { nodes } => nodes,
            other => panic!("expected nodes, got {other:?}"),
        }
    }

    async fn reload(&self) {
        assert_eq!(self.run(Command::ReloadConfig).await, Response::Ok);
    }
}

fn open(config_dir: &Path, secrets: Arc<MemoryStore>) -> Core {
    Core::open(CoreOptions {
        config_dir: config_dir.to_path_buf(),
        secrets,
        state_dir: config_dir.join("state"),
        ssh_config: None,
    })
    .unwrap()
}

fn node(response: Response) -> Node {
    match response {
        Response::Node { node } => node,
        other => panic!("expected a node, got {other:?}"),
    }
}

fn drain(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

#[tokio::test]
async fn creates_folders_and_leaves_on_disk() {
    let f = Fixture::new();
    let work = f.create(None, "folder", "Work").await;
    let host = f.create(Some(work.id), "ssh-host", "Prod Web").await;

    assert!(f.tree().join("work/_folder.toml").is_file());
    let text = fs::read_to_string(f.tree().join("work/prod-web.toml")).unwrap();
    assert!(text.contains(&host.id.to_string()));
    assert!(text.contains("kind = \"ssh-host\""));
    assert_eq!(host.parent, Some(work.id));

    // A fresh core sees the same tree, with the same ids.
    let reopened = open(f.dir.path(), f.secrets.clone());
    let nodes = match reopened.execute(Command::ListNodes).await.unwrap() {
        Response::Nodes { nodes } => nodes,
        other => panic!("{other:?}"),
    };
    assert_eq!(nodes, f.nodes().await);
    assert_eq!(nodes.len(), 2);
}

#[tokio::test]
async fn names_that_clash_get_numbered_files() {
    let f = Fixture::new();
    f.create(None, "ssh-host", "web").await;
    f.create(None, "ssh-host", "Web").await;
    assert!(f.tree().join("web.toml").is_file());
    assert!(f.tree().join("web-2.toml").is_file());
}

#[tokio::test]
async fn renaming_a_folder_keeps_its_children_reachable() {
    let f = Fixture::new();
    let folder = f.create(None, "folder", "Old").await;
    let child = f.create(Some(folder.id), "ssh-host", "db").await;

    f.run(Command::UpdateNode {
        id: folder.id,
        patch: NodePatch {
            name: Some("New".into()),
            ..Default::default()
        },
    })
    .await;
    assert!(f.tree().join("new/db.toml").is_file());
    assert!(!f.tree().join("old").exists());

    // The child's stored path must have followed the rename.
    f.run(Command::UpdateNode {
        id: child.id,
        patch: NodePatch {
            color: Some(Some("#e06c75".into())),
            ..Default::default()
        },
    })
    .await;
    let text = fs::read_to_string(f.tree().join("new/db.toml")).unwrap();
    assert!(text.contains("#e06c75"));
}

#[tokio::test]
async fn moves_nodes_but_not_folders_into_themselves() {
    let f = Fixture::new();
    let a = f.create(None, "folder", "a").await;
    let b = f.create(Some(a.id), "folder", "b").await;
    let host = f.create(None, "ssh-host", "host").await;

    let moved = node(
        f.run(Command::MoveNode {
            id: host.id,
            parent: Some(b.id),
        })
        .await,
    );
    assert_eq!(moved.parent, Some(b.id));
    assert!(f.tree().join("a/b/host.toml").is_file());

    let err = f
        .core
        .execute(Command::MoveNode {
            id: a.id,
            parent: Some(b.id),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::MoveIntoSelf));

    let leaf = f.create(None, "ssh-host", "leaf").await;
    let err = f
        .core
        .execute(Command::MoveNode {
            id: a.id,
            parent: Some(leaf.id),
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotAFolder(_)));
}

#[tokio::test]
async fn deleting_a_folder_needs_recursive_and_removes_secrets() {
    let f = Fixture::new();
    let folder = f.create(None, "folder", "f").await;
    let host = f.create(Some(folder.id), "ssh-host", "h").await;
    f.run(Command::SetSecret {
        id: host.id,
        field: "password".into(),
        value: SecretValue::new("hunter2"),
    })
    .await;
    assert_eq!(f.secrets.len(), 1);

    let err = f
        .core
        .execute(Command::DeleteNode {
            id: folder.id,
            recursive: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, Error::FolderNotEmpty(_)));

    let mut rx = f.core.subscribe();
    f.run(Command::DeleteNode {
        id: folder.id,
        recursive: true,
    })
    .await;
    assert_eq!(
        drain(&mut rx),
        vec![
            Event::NodeDeleted { id: host.id },
            Event::NodeDeleted { id: folder.id },
        ]
    );
    assert!(f.secrets.is_empty());
    assert!(!f.tree().join("f").exists());
}

#[tokio::test]
async fn secrets_stay_out_of_config_files_and_the_api() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "h").await;

    let updated = node(
        f.run(Command::SetSecret {
            id: host.id,
            field: "password".into(),
            value: SecretValue::new("hunter2"),
        })
        .await,
    );
    assert!(updated.secrets.contains("password"));
    assert!(!serde_json::to_string(&updated).unwrap().contains("hunter2"));

    let text = fs::read_to_string(f.tree().join("h.toml")).unwrap();
    assert!(!text.contains("hunter2"));
    assert!(text.contains(&format!("keyring:burrow/{}/password", host.id)));

    let value = f.core.resolve_secret(host.id, "password").await.unwrap();
    assert_eq!(value.unwrap().expose(), "hunter2");

    f.run(Command::ClearSecret {
        id: host.id,
        field: "password".into(),
    })
    .await;
    assert!(f.secrets.is_empty());
    assert!(
        f.core
            .resolve_secret(host.id, "password")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn foreign_secret_references_are_never_followed() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "h").await;
    let victim = burrow_core::SecretKey {
        service: "other-app".into(),
        user: "token".into(),
    };
    burrow_core::SecretStore::set(&*f.secrets, &victim, &SecretValue::new("theirs")).unwrap();

    // Point the node's password at another app's keyring entry by hand.
    let path = f.tree().join("h.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        format!("{text}\n[secrets]\npassword = \"keyring:other-app/token\"\n"),
    )
    .unwrap();
    f.reload().await;

    let problems = match f.run(Command::ListProblems).await {
        Response::Problems { problems } => problems,
        other => panic!("{other:?}"),
    };
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        f.core
            .resolve_secret(host.id, "password")
            .await
            .unwrap()
            .is_none()
    );

    f.run(Command::DeleteNode {
        id: host.id,
        recursive: false,
    })
    .await;
    let still_there = burrow_core::SecretStore::get(&*f.secrets, &victim).unwrap();
    assert_eq!(still_there.unwrap().expose(), "theirs");
}

#[tokio::test]
async fn deleting_a_copied_file_keeps_the_original_secret() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "a").await;
    f.run(Command::SetSecret {
        id: host.id,
        field: "password".into(),
        value: SecretValue::new("hunter2"),
    })
    .await;
    fs::copy(f.tree().join("a.toml"), f.tree().join("b.toml")).unwrap();
    f.reload().await;

    let copy = f
        .nodes()
        .await
        .into_iter()
        .find(|n| n.id != host.id)
        .unwrap();
    assert!(copy.secrets.is_empty());
    f.run(Command::DeleteNode {
        id: copy.id,
        recursive: false,
    })
    .await;
    let value = f.core.resolve_secret(host.id, "password").await.unwrap();
    assert_eq!(value.unwrap().expose(), "hunter2");
}

#[tokio::test]
async fn rejects_bad_input() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "h").await;
    let bad = [
        Command::CreateNode {
            parent: None,
            kind: "SSH".into(),
            name: "x".into(),
            color: None,
            props: toml::Table::new(),
        },
        Command::CreateNode {
            parent: None,
            kind: "ssh-host".into(),
            name: "   ".into(),
            color: None,
            props: toml::Table::new(),
        },
        Command::CreateNode {
            parent: Some(host.id),
            kind: "ssh-host".into(),
            name: "inside a leaf".into(),
            color: None,
            props: toml::Table::new(),
        },
        Command::SetSecret {
            id: host.id,
            field: "../escape".into(),
            value: SecretValue::new("x"),
        },
        Command::GetNode { id: NodeId::new() },
    ];
    for command in bad {
        assert!(
            f.core.execute(command.clone()).await.is_err(),
            "{command:?} should fail"
        );
    }
}

#[tokio::test]
async fn hand_written_files_get_an_id_once() {
    let f = Fixture::new();
    fs::create_dir(f.tree().join("lab")).unwrap();
    fs::write(f.tree().join("lab/pi.toml"), "kind = \"ssh-host\"\n").unwrap();

    let mut rx = f.core.subscribe();
    f.reload().await;
    let events = drain(&mut rx);
    assert_eq!(events.len(), 2, "{events:?}");
    // Parents are announced before their children.
    let Event::NodeCreated { node: folder } = &events[0] else {
        panic!("{events:?}")
    };
    let Event::NodeCreated { node: leaf } = &events[1] else {
        panic!("{events:?}")
    };
    assert_eq!(folder.name, "lab");
    assert_eq!(leaf.name, "pi");
    assert_eq!(leaf.parent, Some(folder.id));

    // The ids were written back, so reloading again changes nothing.
    assert!(f.tree().join("lab/_folder.toml").is_file());
    assert!(
        fs::read_to_string(f.tree().join("lab/pi.toml"))
            .unwrap()
            .contains("id = ")
    );
    f.reload().await;
    assert_eq!(drain(&mut rx), vec![]);
}

#[tokio::test]
async fn folder_names_keep_their_dots() {
    let f = Fixture::new();
    fs::create_dir(f.tree().join("example.com")).unwrap();
    f.reload().await;
    let folder = f.nodes().await.pop().unwrap();
    assert_eq!(folder.name, "example.com");

    // Moving it keeps the directory name too.
    let other = f.create(None, "folder", "other").await;
    f.run(Command::MoveNode {
        id: folder.id,
        parent: Some(other.id),
    })
    .await;
    assert!(f.tree().join("other/example.com/_folder.toml").is_file());
}

#[tokio::test]
async fn our_own_writes_reload_to_no_events() {
    let f = Fixture::new();
    let folder = f.create(None, "folder", "f").await;
    f.create(Some(folder.id), "ssh-host", "h").await;
    let mut rx = f.core.subscribe();
    f.reload().await;
    assert_eq!(drain(&mut rx), vec![]);
}

#[tokio::test]
async fn copied_files_get_a_new_id() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "a").await;
    fs::copy(f.tree().join("a.toml"), f.tree().join("b.toml")).unwrap();
    f.reload().await;

    let nodes = f.nodes().await;
    assert_eq!(nodes.len(), 2);
    assert_ne!(nodes[0].id, nodes[1].id);
    // The original (first in path order) keeps its id.
    assert!(nodes.iter().any(|n| n.id == host.id && n.name == "a"));
}

#[tokio::test]
async fn broken_files_are_reported_and_skipped() {
    let f = Fixture::new();
    f.create(None, "ssh-host", "good").await;
    fs::write(f.tree().join("broken.toml"), "kind = \n").unwrap();
    fs::write(f.tree().join("kindless.toml"), "name = \"x\"\n").unwrap();

    let mut rx = f.core.subscribe();
    f.reload().await;
    let problems = match f.run(Command::ListProblems).await {
        Response::Problems { problems } => problems,
        other => panic!("{other:?}"),
    };
    assert_eq!(problems.len(), 2, "{problems:?}");
    assert!(matches!(
        &drain(&mut rx)[..],
        [Event::ProblemsChanged { .. }]
    ));
    assert_eq!(f.nodes().await.len(), 1);
    // A broken file is never overwritten.
    assert_eq!(
        fs::read_to_string(f.tree().join("broken.toml")).unwrap(),
        "kind = \n"
    );

    fs::remove_file(f.tree().join("broken.toml")).unwrap();
    fs::remove_file(f.tree().join("kindless.toml")).unwrap();
    f.reload().await;
    assert_eq!(
        drain(&mut rx),
        vec![Event::ProblemsChanged { problems: vec![] }]
    );
}

#[tokio::test]
async fn files_deleted_outside_burrow_send_events() {
    let f = Fixture::new();
    let host = f.create(None, "ssh-host", "h").await;
    fs::remove_file(f.tree().join("h.toml")).unwrap();
    let mut rx = f.core.subscribe();
    f.reload().await;
    assert_eq!(drain(&mut rx), vec![Event::NodeDeleted { id: host.id }]);
}

/// The one test of the real file watcher. Generous timeout: watchers can be
/// slow on CI machines.
#[tokio::test(flavor = "multi_thread")]
async fn watcher_picks_up_external_edits() {
    let f = Fixture::new();
    let _watcher = f.core.watch().unwrap();
    let mut rx = f.core.subscribe();

    fs::write(f.tree().join("edited.toml"), "kind = \"ssh-host\"\n").unwrap();

    let event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await.unwrap() {
                Event::NodeCreated { node } => return node,
                _ => continue,
            }
        }
    })
    .await
    .expect("watcher did not report the new file within 10s");
    assert_eq!(event.name, "edited");
}

#[test]
fn commands_have_a_stable_json_shape() {
    let command: Command = serde_json::from_str(
        r#"{"type":"create_node","kind":"ssh-host","name":"web","props":{"alias":"web"}}"#,
    )
    .unwrap();
    assert!(matches!(command, Command::CreateNode { parent: None, .. }));

    let patch: NodePatch = serde_json::from_str(r#"{"color":null}"#).unwrap();
    assert_eq!(patch.color, Some(None));
    let patch: NodePatch = serde_json::from_str(r#"{}"#).unwrap();
    assert_eq!(patch.color, None);

    let json = serde_json::to_string(&Response::Version { api: 1 }).unwrap();
    assert_eq!(json, r#"{"type":"version","api":1}"#);
}

#[test]
fn secret_values_are_redacted_in_debug_output() {
    let command = Command::SetSecret {
        id: NodeId::new(),
        field: "password".into(),
        value: SecretValue::new("hunter2"),
    };
    assert!(!format!("{command:?}").contains("hunter2"));
}

/// A core pointed at its own OpenSSH config, as `BURROW_SSH_CONFIG` does.
fn with_ssh_config(dir: &Path) -> (Core, std::path::PathBuf) {
    let ssh_config = dir.join("ssh/config");
    fs::create_dir_all(ssh_config.parent().unwrap()).unwrap();
    fs::write(&ssh_config, "Host *\n    Port 2\n").unwrap();
    let core = Core::open(CoreOptions {
        config_dir: dir.to_path_buf(),
        secrets: Arc::new(MemoryStore::new()),
        state_dir: dir.join("state"),
        ssh_config: Some(ssh_config.clone()),
    })
    .unwrap();
    (core, ssh_config)
}

fn add_host(parent: Option<NodeId>, alias: &str, add_include: bool) -> Command {
    Command::AddSshHost {
        parent,
        alias: alias.into(),
        hostname: "10.0.0.5".into(),
        user: Some("deploy".into()),
        port: None,
        identity_file: None,
        add_include,
    }
}

#[tokio::test]
async fn a_host_added_in_burrow_is_a_node_and_an_ssh_host() {
    let dir = TempDir::new().unwrap();
    let (core, ssh_config) = with_ssh_config(dir.path());
    let hosts = dir.path().join("ssh/config.d/burrow.conf");
    let mut events = core.subscribe();

    let Response::SshInclude {
        included,
        config,
        line,
    } = core
        .execute(Command::CheckSshInclude {
            alias: "web".into(),
        })
        .await
        .unwrap()
    else {
        panic!("expected the include status");
    };
    assert!(!included);
    assert_eq!(config, ssh_config);
    assert_eq!(line, format!("Include {}", hosts.display()));

    let folder = node(
        core.execute(Command::CreateNode {
            parent: None,
            kind: "folder".into(),
            name: "Work".into(),
            color: None,
            props: toml::Table::new(),
        })
        .await
        .unwrap(),
    );
    let web = node(
        core.execute(add_host(Some(folder.id), "web", true))
            .await
            .unwrap(),
    );
    assert_eq!(web.kind, "ssh-host");
    assert_eq!(web.name, "web");
    assert_eq!(web.parent, Some(folder.id));
    assert_eq!(web.props.get("alias").and_then(|v| v.as_str()), Some("web"));
    let mut created = Vec::new();
    while let Ok(Event::NodeCreated { node }) = events.try_recv() {
        created.push(node.id);
    }
    assert_eq!(created, [folder.id, web.id]);

    let config = fs::read_to_string(&ssh_config).unwrap();
    assert!(config.starts_with("# Added by Burrow"), "{config}");
    assert!(config.ends_with("Host *\n    Port 2\n"), "{config}");
    let output = std::process::Command::new("ssh")
        .arg("-G")
        .arg("-F")
        .arg(&ssh_config)
        .arg("web")
        .output()
        .unwrap();
    let output = String::from_utf8_lossy(&output.stdout);
    for line in ["hostname 10.0.0.5", "user deploy", "port 2"] {
        assert!(output.lines().any(|l| l == line), "{line} in {output}");
    }

    // The same alias again is refused, and the node stays the only one.
    let error = core
        .execute(add_host(None, "web", false))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("already added in Burrow"),
        "{error}"
    );
}

#[tokio::test]
async fn a_host_that_cannot_become_a_node_writes_nothing() {
    let dir = TempDir::new().unwrap();
    let (core, ssh_config) = with_ssh_config(dir.path());
    let leaf = node(
        core.execute(Command::CreateNode {
            parent: None,
            kind: "ssh-host".into(),
            name: "leaf".into(),
            color: None,
            props: toml::Table::new(),
        })
        .await
        .unwrap(),
    );
    let error = core
        .execute(add_host(Some(leaf.id), "web", true))
        .await
        .unwrap_err();
    assert!(matches!(error, Error::NotAFolder(_)), "{error}");
    assert_eq!(
        fs::read_to_string(&ssh_config).unwrap(),
        "Host *\n    Port 2\n"
    );
    assert!(!dir.path().join("ssh/config.d").exists());
}
