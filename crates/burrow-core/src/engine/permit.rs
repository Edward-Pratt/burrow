//! What an addon may send: the rule its command's class names, checked
//! against the permissions and node kinds in the addon's manifest.

use crate::addon::{Manifest, Permission};
use crate::api::Command;
use crate::class::AddonRule;
use crate::error::{Error, Result};

use super::Inner;
use super::checks::command_name;

impl Inner {
    /// Whether an addon may send `command`, by what its class says it
    /// needs (see [`AddonRule`]) and the addon's permissions and kinds.
    pub(super) fn permit(&self, manifest: &Manifest, command: &Command) -> Result<()> {
        let need = |permission: Permission| {
            if manifest.has(permission) {
                Ok(())
            } else {
                Err(Error::PermissionDenied(format!(
                    "addon {} lacks {permission}",
                    manifest.id
                )))
            }
        };
        let own = |kind: &str| {
            if manifest.owns(kind) {
                Ok(())
            } else {
                Err(Error::PermissionDenied(format!(
                    "addon {} may only change its own kinds of node, not {kind}",
                    manifest.id
                )))
            }
        };
        match command.class().addon {
            AddonRule::Free => Ok(()),
            AddonRule::ReadNodes => need(Permission::NodesRead),
            AddonRule::CreateNode(kind) => {
                need(Permission::NodesWrite)?;
                own(kind)
            }
            AddonRule::ChangeNode(id) => {
                need(Permission::NodesWrite)?;
                own(&self.tree().get(id)?.node.kind)
            }
            AddonRule::Denied => Err(Error::PermissionDenied(format!(
                "addons may not send {}",
                command_name(command)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{NodeId, SessionId};
    use crate::engine::{Core, CoreOptions};
    use std::sync::Arc;

    /// A core with one addon that may read and write nodes.
    fn core_with_addon() -> (tempfile::TempDir, Core, Manifest) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let folder = dir.path().join("config/addons/probe");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("addon.toml"),
            "id = \"probe\"\nname = \"Probe\"\nversion = \"0.1.0\"\napi = \"1\"\n\
             permissions = [\"nodes:read\", \"nodes:write\"]\n\
             [backend]\ncommand = \"probe\"\nprotocol = \"jsonrpc-stdio\"\n",
        )
        .unwrap();
        std::fs::write(folder.join("probe"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(folder.join("probe"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let manifest = crate::addon::load_manifest(&folder).unwrap();
        let core = Core::open(CoreOptions {
            config_dir: dir.path().join("config"),
            state_dir: dir.path().join("state"),
            secrets: Arc::new(crate::secrets::MemoryStore::new()),
            ssh_config: None,
        })
        .unwrap();
        (dir, core, manifest)
    }

    #[test]
    fn addons_may_not_reach_databases() {
        let (_dir, core, manifest) = core_with_addon();
        let session = SessionId(1);
        let node = NodeId::new();
        for command in [
            Command::OpenDatabase { node },
            Command::RunQuery {
                session,
                sql: "select 1".into(),
                limit: None,
            },
            Command::CancelQuery { session },
            Command::SetReadOnly {
                session,
                read_only: false,
            },
            Command::ListSchema { session },
            Command::ExportQuery {
                session,
                sql: "select 1".into(),
                path: "/tmp/out.csv".into(),
            },
            Command::ListHistory { node },
            Command::ClearHistory { node },
        ] {
            let error = core.inner.permit(&manifest, &command).unwrap_err();
            assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        }
    }

    #[test]
    fn addons_may_not_add_ssh_hosts() {
        let (_dir, core, manifest) = core_with_addon();
        for command in [
            Command::CheckSshInclude {
                alias: "web".into(),
            },
            Command::AddSshHost {
                parent: None,
                alias: "web".into(),
                hostname: "10.0.0.5".into(),
                user: None,
                port: None,
                identity_file: None,
                add_include: true,
            },
        ] {
            let error = core.inner.permit(&manifest, &command).unwrap_err();
            assert!(matches!(error, Error::PermissionDenied(_)), "{error}");
        }
    }
}
