//! End-to-end tests of the addon host, with the notes addon as the
//! sidecar. Each test installs a copy of it into a temporary config folder.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use burrow_core::{
    AddonInfo, AddonState, Block, Command, Core, CoreOptions, Error, Event, FormValue, MemoryStore,
    Node, NodeId, Response, View,
};
use tempfile::TempDir;
use tokio::sync::broadcast;

const PROGRAM: &str = env!("CARGO_BIN_EXE_burrow-addon-notes");
const MANIFEST: &str = include_str!("../addon.toml");

struct Fixture {
    dir: TempDir,
    core: Core,
}

impl Fixture {
    fn new() -> Self {
        Self::with(|_| {})
    }

    /// Installs the addon, lets `setup` change the folder, then opens a
    /// core on it.
    fn with(setup: impl FnOnce(&Path)) -> Self {
        let dir = TempDir::new().unwrap();
        install(&dir.path().join("addons"), "notes", MANIFEST);
        setup(dir.path());
        let core = Core::open(CoreOptions {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().join("state"),
            secrets: Arc::new(MemoryStore::new()),
            ssh_config: None,
        })
        .unwrap();
        Fixture { dir, core }
    }

    fn program(&self) -> PathBuf {
        self.dir.path().join("addons/notes/burrow-addon-notes")
    }

    async fn run(&self, command: Command) -> burrow_core::Result<Response> {
        self.core.execute(command).await
    }

    async fn addons(&self) -> Vec<AddonInfo> {
        match self.run(Command::ListAddons).await.unwrap() {
            Response::Addons { addons } => addons,
            other => panic!("expected addons, got {other:?}"),
        }
    }

    async fn state(&self) -> AddonState {
        self.addons().await.remove(0).state
    }

    async fn approve(&self) {
        let fingerprint = self.addons().await.remove(0).fingerprint;
        self.run(Command::ApproveAddon {
            id: "notes".into(),
            fingerprint,
        })
        .await
        .unwrap();
    }

    async fn create(&self, kind: &str, name: &str) -> Node {
        let mut props = toml::Table::new();
        if kind == "ssh-host" {
            props.insert("alias".into(), "web.example".into());
        }
        let response = self
            .run(Command::CreateNode {
                parent: None,
                kind: kind.into(),
                name: name.into(),
                color: None,
                props,
            })
            .await
            .unwrap();
        match response {
            Response::Node { node } => node,
            other => panic!("expected a node, got {other:?}"),
        }
    }

    async fn get(&self, id: NodeId) -> Node {
        match self.run(Command::GetNode { id }).await.unwrap() {
            Response::Node { node } => node,
            other => panic!("expected a node, got {other:?}"),
        }
    }

    async fn view(&self, node: NodeId) -> burrow_core::Result<View> {
        self.run(Command::OpenView { node }).await.map(|r| match r {
            Response::View { view } => view,
            other => panic!("expected a view, got {other:?}"),
        })
    }

    async fn invoke(
        &self,
        action: &str,
        node: Option<NodeId>,
        values: &[(&str, &str)],
    ) -> burrow_core::Result<Option<String>> {
        let values: BTreeMap<String, FormValue> = values
            .iter()
            .map(|(k, v)| (k.to_string(), FormValue::Text(v.to_string())))
            .collect();
        let response = self
            .run(Command::InvokeAction {
                addon: "notes".into(),
                action: action.into(),
                node,
                values,
            })
            .await?;
        match response {
            Response::Invoked { message } => Ok(message),
            other => panic!("expected invoked, got {other:?}"),
        }
    }
}

fn install(addons: &Path, id: &str, manifest: &str) {
    let folder = addons.join(id);
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("addon.toml"), manifest).unwrap();
    fs::copy(PROGRAM, folder.join("burrow-addon-notes")).unwrap();
}

async fn next_view(events: &mut broadcast::Receiver<Event>) -> (NodeId, View) {
    let wait = async {
        loop {
            if let Event::ViewChanged { node, view } = events.recv().await.unwrap() {
                return (node, view);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(10), wait)
        .await
        .expect("no view_changed event")
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unapproved_addon_never_starts() {
    let f = Fixture::new();
    assert_eq!(
        f.state().await,
        AddonState::NeedsApproval { changed: false }
    );

    let note = f.create("note", "Shopping").await;
    let error = f.view(note.id).await.unwrap_err();
    assert!(matches!(error, Error::AddonNotApproved(_)), "{error}");
    let error = f.invoke("count", None, &[]).await.unwrap_err();
    assert!(matches!(error, Error::AddonNotApproved(_)), "{error}");
    assert_eq!(
        f.state().await,
        AddonState::NeedsApproval { changed: false }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_needs_the_current_fingerprint() {
    let f = Fixture::new();
    let error = f
        .run(Command::ApproveAddon {
            id: "notes".into(),
            fingerprint: "0".repeat(64),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changed"), "{error}");
    assert_eq!(
        f.state().await,
        AddonState::NeedsApproval { changed: false }
    );

    f.approve().await;
    assert_eq!(f.state().await, AddonState::Ready);
    // The approval is kept in the state folder, not the config folder.
    let grants = fs::read_to_string(f.dir.path().join("state/addons.toml")).unwrap();
    assert!(grants.contains("notes"));
}

#[tokio::test(flavor = "multi_thread")]
async fn views_and_forms_round_trip() {
    let f = Fixture::new();
    f.approve().await;
    let mut events = f.core.subscribe();
    let note = f.create("note", "Shopping").await;

    let view = f.view(note.id).await.unwrap();
    assert_eq!(
        view.blocks[0],
        Block::Heading {
            text: "Shopping".into()
        }
    );
    assert_eq!(f.state().await, AddonState::Running);

    let message = f.invoke("save", Some(note.id), &[("text", "eggs")]).await;
    assert_eq!(message.unwrap().as_deref(), Some("Saved"));
    assert_eq!(f.get(note.id).await.props["text"].as_str(), Some("eggs"));

    let (node, view) = next_view(&mut events).await;
    assert_eq!(node, note.id);
    assert!(view.blocks.contains(&Block::Text {
        text: "eggs".into()
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn commands_and_actions_must_be_offered() {
    let f = Fixture::new();
    f.approve().await;

    let message = f.invoke("new-note", None, &[]).await.unwrap();
    assert!(message.unwrap().contains("New note"));
    let message = f.invoke("count", None, &[]).await.unwrap();
    assert_eq!(message.as_deref(), Some("1 note"));

    let host = f.create("ssh-host", "web").await;
    let message = f.invoke("describe", Some(host.id), &[]).await.unwrap();
    assert_eq!(
        message.as_deref(),
        Some("web connects to the alias web.example")
    );

    // Not a palette command, and not an action offered on SSH hosts.
    for node in [None, Some(host.id)] {
        let error = f.invoke("crash", node, &[]).await.unwrap_err();
        assert!(error.to_string().contains("offers no"), "{error}");
    }
    assert_eq!(f.state().await, AddonState::Running);
}

#[tokio::test(flavor = "multi_thread")]
async fn addons_only_change_their_own_kinds() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Mine").await;
    let host = f.create("ssh-host", "web").await;

    let target = host.id.to_string();
    let error = f
        .invoke("trespass", Some(note.id), &[("target", &target)])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("permission denied"), "{error}");
    assert_eq!(f.get(host.id).await.name, "web");

    let target = note.id.to_string();
    let message = f
        .invoke("trespass", Some(note.id), &[("target", &target)])
        .await;
    assert_eq!(message.unwrap().as_deref(), Some("Renamed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn permissions_are_enforced() {
    let f = Fixture::with(|dir| {
        let manifest = MANIFEST.replace(r#", "nodes:write""#, "");
        fs::write(dir.join("addons/notes/addon.toml"), manifest).unwrap();
    });
    f.approve().await;

    let error = f.invoke("new-note", None, &[]).await.unwrap_err();
    assert!(error.to_string().contains("lacks nodes:write"), "{error}");
    // Reading is still allowed.
    let message = f.invoke("count", None, &[]).await.unwrap();
    assert_eq!(message.as_deref(), Some("0 notes"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_program_needs_approval_again() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;

    // Changed while not running, and before any rescan: caught at start.
    let mut bytes = fs::read(f.program()).unwrap();
    bytes.extend_from_slice(b"tampered");
    fs::write(f.program(), &bytes).unwrap();
    let error = f.view(note.id).await.unwrap_err();
    assert!(matches!(error, Error::AddonNotApproved(_)), "{error}");
    assert_eq!(f.state().await, AddonState::NeedsApproval { changed: true });

    // Approving the new program lets it run.
    f.approve().await;
    f.view(note.id).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rescan_stops_a_changed_addon() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;
    f.view(note.id).await.unwrap();
    assert_eq!(f.state().await, AddonState::Running);

    let manifest = MANIFEST.replace(r#""nodes:read", "#, "");
    fs::write(f.dir.path().join("addons/notes/addon.toml"), manifest).unwrap();
    f.run(Command::ReloadConfig).await.unwrap();
    assert_eq!(f.state().await, AddonState::NeedsApproval { changed: true });
    assert!(f.view(note.id).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_stops_the_addon() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;
    f.view(note.id).await.unwrap();

    f.run(Command::RevokeAddon { id: "notes".into() })
        .await
        .unwrap();
    assert_eq!(
        f.state().await,
        AddonState::NeedsApproval { changed: false }
    );
    let error = f.view(note.id).await.unwrap_err();
    assert!(matches!(error, Error::AddonNotApproved(_)), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_crashed_addon_restarts_after_a_delay() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;

    let error = f.invoke("crash", Some(note.id), &[]).await.unwrap_err();
    assert!(error.to_string().contains("stopped"), "{error}");
    // The supervisor reports the exit on its own task.
    for _ in 0..50 {
        if matches!(f.state().await, AddonState::Failed { .. }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(matches!(f.state().await, AddonState::Failed { .. }));

    let error = f.view(note.id).await.unwrap_err();
    assert!(error.to_string().contains("trying again"), "{error}");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    f.view(note.id).await.unwrap();
    assert_eq!(f.state().await, AddonState::Running);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversized_message_stops_the_addon() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;

    assert!(f.invoke("flood", Some(note.id), &[]).await.is_err());
    match f.state().await {
        AddonState::Failed { error } => assert!(error.contains("over"), "{error}"),
        other => panic!("expected failed, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_addon_by_id_keeps_a_contested_kind() {
    let f = Fixture::with(|dir| {
        let manifest = MANIFEST.replace(r#"id = "notes""#, r#"id = "notes2""#);
        install(&dir.join("addons"), "notes2", &manifest);
    });
    let addons = f.addons().await;
    assert_eq!(addons.len(), 1);
    assert_eq!(addons[0].id, "notes");

    let problems = match f.run(Command::ListProblems).await.unwrap() {
        Response::Problems { problems } => problems,
        other => panic!("expected problems, got {other:?}"),
    };
    assert_eq!(problems.len(), 1);
    assert!(problems[0].path.ends_with("notes2/addon.toml"));
    assert!(
        problems[0]
            .message
            .contains("already belongs to addon notes")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn addons_write_to_their_data_folder_and_stay_approved() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;

    let cwd = f
        .invoke("touch", Some(note.id), &[])
        .await
        .unwrap()
        .unwrap();
    let data = f.dir.path().join("state/addons/notes");
    assert_eq!(Path::new(&cwd), data.canonicalize().unwrap());
    assert!(data.join("scratch").exists());
    assert!(!f.dir.path().join("addons/notes/scratch").exists());

    f.run(Command::ReloadConfig).await.unwrap();
    assert_eq!(f.state().await, AddonState::Running);
}

/// The event types the addon has received, once `expected` shows up or
/// after a second.
async fn events_seen(f: &Fixture, note: NodeId, expected: &str) -> String {
    let mut seen = String::new();
    for _ in 0..50 {
        seen = f.invoke("events", Some(note), &[]).await.unwrap().unwrap();
        if seen.contains(expected) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    seen
}

#[tokio::test(flavor = "multi_thread")]
async fn node_events_need_nodes_read() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Shopping").await;
    f.view(note.id).await.unwrap();
    f.create("ssh-host", "web").await;
    assert_eq!(
        events_seen(&f, note.id, "node_created").await,
        "node_created"
    );

    let f = Fixture::with(|dir| {
        let manifest = MANIFEST.replace(r#""nodes:read", "#, "");
        fs::write(dir.join("addons/notes/addon.toml"), manifest).unwrap();
    });
    f.approve().await;
    let note = f.create("note", "Shopping").await;
    f.view(note.id).await.unwrap();
    f.create("ssh-host", "web").await;
    // Give a wrongly forwarded event time to arrive.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(events_seen(&f, note.id, "never").await, "");
}

#[tokio::test(flavor = "multi_thread")]
async fn views_can_only_be_redrawn_for_own_nodes() {
    let f = Fixture::new();
    f.approve().await;
    let note = f.create("note", "Mine").await;
    let host = f.create("ssh-host", "web").await;

    let target = host.id.to_string();
    let error = f
        .invoke("redraw", Some(note.id), &[("target", &target)])
        .await
        .unwrap_err();
    assert!(error.to_string().contains("permission denied"), "{error}");

    let mut events = f.core.subscribe();
    let target = note.id.to_string();
    let message = f
        .invoke("redraw", Some(note.id), &[("target", &target)])
        .await;
    assert_eq!(message.unwrap().as_deref(), Some("Redrawn"));
    assert_eq!(next_view(&mut events).await.0, note.id);
}
