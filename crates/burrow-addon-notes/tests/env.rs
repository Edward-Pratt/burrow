//! Addons must not inherit `SSH_AUTH_SOCK`. This sets an environment
//! variable, so it is its own test binary: changing the environment while
//! other threads start processes is not safe.

use std::fs;
use std::sync::Arc;

use burrow_core::{Command, Core, CoreOptions, MemoryStore, Response};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread")]
async fn addons_do_not_inherit_ssh_auth_sock() {
    // Safe: this is the only test in the binary, and no other thread is
    // running yet.
    unsafe { std::env::set_var("SSH_AUTH_SOCK", "/tmp/agent.sock") };

    let dir = TempDir::new().unwrap();
    let folder = dir.path().join("addons/notes");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("addon.toml"), include_str!("../addon.toml")).unwrap();
    fs::copy(
        env!("CARGO_BIN_EXE_burrow-addon-notes"),
        folder.join("burrow-addon-notes"),
    )
    .unwrap();
    let core = Core::open(CoreOptions {
        config_dir: dir.path().to_path_buf(),
        state_dir: dir.path().join("state"),
        secrets: Arc::new(MemoryStore::new()),
        ssh_config: None,
    })
    .unwrap();

    let Response::Addons { mut addons } = core.execute(Command::ListAddons).await.unwrap() else {
        panic!("expected addons");
    };
    let fingerprint = addons.remove(0).fingerprint;
    core.execute(Command::ApproveAddon {
        id: "notes".into(),
        fingerprint,
    })
    .await
    .unwrap();
    let Response::Node { node } = core
        .execute(Command::CreateNode {
            parent: None,
            kind: "note".into(),
            name: "n".into(),
            color: None,
            props: toml::Table::new(),
        })
        .await
        .unwrap()
    else {
        panic!("expected a node");
    };

    let response = core
        .execute(Command::InvokeAction {
            addon: "notes".into(),
            action: "env".into(),
            node: Some(node.id),
            values: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(
        response,
        Response::Invoked {
            message: Some(String::new())
        }
    );
}
