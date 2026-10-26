//! Addons: sidecar programs that speak JSON-RPC over stdio. Each lives in
//! `addons/<id>/` next to the tree, with an `addon.toml` manifest.
//!
//! An addon only runs once the user has approved it. Approvals live in the
//! state folder, not the config folder, because the config folder can come
//! from a dotfiles repo or a shared folder: an addon that arrives with a
//! `git pull` must never start by itself. An approval is pinned to the
//! manifest's [`fingerprint`](crate::AddonInfo::fingerprint), which covers
//! every file in the addon's folder, so a changed program, or a changed
//! script it loads, needs approving again. The folder is checked again
//! just before each start. A file changed in the instant between that check
//! and the program reading it is not caught; doing that needs a writer
//! racing Burrow on this machine, which already runs as the user.
//!
//! Addons start on first use. One that stops is started again on the next
//! use, after a delay that doubles each time it stops soon after starting.
//!
//! Since writing into its folder would withdraw an addon's approval, each
//! addon runs in a data folder of its own under the state folder instead.

mod manifest;
mod rpc;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

use crate::api::{AddonInfo, AddonState, Event, FormValue, Node, Problem, View};
use crate::engine::Core;
use crate::error::{Error, Result};
use crate::pins::Pinned;

#[cfg(test)]
pub(crate) use manifest::load as load_manifest;
pub(crate) use manifest::{Manifest, Permission};
use rpc::Process;

/// The first delay before restarting an addon that stopped, and the cap.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

pub(crate) struct Host {
    dir: PathBuf,
    /// Addon id to the fingerprint the user approved, in the state file's
    /// `[approved]` table. Changed only while `state` is held, so a check
    /// under that lock sees the addon and its approval together.
    approvals: Pinned<String, String>,
    /// Holds each addon's writable data folder, by id.
    data_dir: PathBuf,
    state: Mutex<State>,
    /// Held while starting an addon, so two first uses start it once. One
    /// per addon, so one that is slow to start holds up no other.
    starting: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

#[derive(Default)]
struct State {
    addons: BTreeMap<String, Addon>,
    /// Which addon owns each kind.
    kinds: HashMap<String, String>,
    problems: Vec<Problem>,
    next_generation: u64,
}

struct Addon {
    manifest: Arc<Manifest>,
    process: Option<Arc<Process>>,
    failure: Option<Failure>,
}

struct Failure {
    error: String,
    retry_at: Instant,
    backoff: Duration,
}

impl Host {
    pub fn new(dir: PathBuf, approvals_path: PathBuf, data_dir: PathBuf) -> Host {
        // `rescan` reads the file again and reports it if it is broken.
        let (approvals, _) = Pinned::load(approvals_path, "approved");
        Host {
            dir,
            approvals,
            data_dir,
            state: Mutex::default(),
            starting: Mutex::default(),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-reads every manifest and the approvals. Addons whose fingerprint
    /// changed are stopped. Returns whether the list of addons changed.
    /// Blocking: reads files and hashes programs.
    pub fn rescan(&self) -> bool {
        let (manifests, mut problems) = manifest::scan(&self.dir);
        let mut state = self.state();
        let before = state.infos(&self.approvals);
        // Read after `before`, under `state`, so a change to the approvals
        // alone still counts as a change.
        if let Some(message) = self.approvals.reload() {
            problems.push(Problem {
                path: self.approvals.path().to_path_buf(),
                message,
            });
        }
        let mut old = std::mem::take(&mut state.addons);
        let mut kinds = HashMap::new();
        // Addons are taken in id order, so when two claim a kind the one
        // whose id sorts first keeps it, on every load.
        for manifest in manifests {
            if let Some(kind) = manifest.kinds.iter().find(|k| kinds.contains_key(&k.kind)) {
                problems.push(Problem {
                    path: manifest.dir.join(manifest::MANIFEST_FILE),
                    message: format!(
                        "kind {:?} already belongs to addon {}",
                        kind.kind, kinds[&kind.kind]
                    ),
                });
                continue;
            }
            for kind in &manifest.kinds {
                kinds.insert(kind.kind.clone(), manifest.id.clone());
            }
            let addon = match old.remove(&manifest.id) {
                Some(addon) if addon.manifest.fingerprint == manifest.fingerprint => Addon {
                    manifest: Arc::new(manifest),
                    ..addon
                },
                // New or changed: anything running belongs to the old one.
                _ => Addon {
                    manifest: Arc::new(manifest),
                    process: None,
                    failure: None,
                },
            };
            state.addons.insert(addon.manifest.id.clone(), addon);
        }
        state.kinds = kinds;
        state.problems = problems;
        // Stop whatever is no longer approved.
        for addon in state.addons.values_mut() {
            if !approved(&self.approvals, &addon.manifest) {
                addon.process = None;
            }
        }
        drop(old);
        state.infos(&self.approvals) != before
    }

    pub fn problems(&self) -> Vec<Problem> {
        self.state().problems.clone()
    }

    pub fn infos(&self) -> Vec<AddonInfo> {
        self.state().infos(&self.approvals)
    }

    /// The addon that owns `kind`.
    pub fn owner(&self, kind: &str) -> Option<Arc<Manifest>> {
        let state = self.state();
        let id = state.kinds.get(kind)?;
        Some(state.addons[id].manifest.clone())
    }

    pub fn manifest(&self, id: &str) -> Result<Arc<Manifest>> {
        self.state()
            .addons
            .get(id)
            .map(|a| a.manifest.clone())
            .ok_or_else(|| Error::AddonNotFound(id.to_string()))
    }

    /// Records an approval. Blocking: writes the approvals file.
    pub fn approve(&self, id: &str, fingerprint: &str) -> Result<()> {
        let state = self.state();
        let addon = state
            .addons
            .get(id)
            .ok_or_else(|| Error::AddonNotFound(id.to_string()))?;
        if addon.manifest.fingerprint != fingerprint {
            return Err(Error::invalid(
                "fingerprint",
                format!("addon {id} changed since it was listed; review it again"),
            ));
        }
        self.approvals
            .insert(id.to_string(), fingerprint.to_string())
    }

    /// Withdraws an approval and stops the addon. Blocking.
    pub fn revoke(&self, id: &str) -> Result<()> {
        let mut state = self.state();
        let addon = state
            .addons
            .get_mut(id)
            .ok_or_else(|| Error::AddonNotFound(id.to_string()))?;
        addon.process = None;
        addon.failure = None;
        self.approvals.remove(&id.to_string())
    }

    /// Asks the addon for a node's view, starting it if needed.
    pub async fn open_view(
        &self,
        core: &Core,
        manifest: &Arc<Manifest>,
        node: Node,
    ) -> Result<View> {
        let process = self.running(core, manifest).await?;
        process
            .call("open_view", json!({ "node": node }))
            .await
            .map_err(|message| addon_error(manifest, message))
    }

    /// Sends an action to the addon, starting it if needed. Returns the
    /// message it wants shown, if any.
    pub async fn invoke(
        &self,
        core: &Core,
        manifest: &Arc<Manifest>,
        action: &str,
        node: Option<Node>,
        values: BTreeMap<String, FormValue>,
    ) -> Result<Option<String>> {
        #[derive(Deserialize)]
        struct Invoked {
            #[serde(default)]
            message: Option<String>,
        }
        let process = self.running(core, manifest).await?;
        let params = json!({ "action": action, "node": node, "values": values });
        let invoked: Option<Invoked> = process
            .call("invoke", params)
            .await
            .map_err(|message| addon_error(manifest, message))?;
        Ok(invoked.and_then(|i| i.message))
    }

    /// The addon's process, started if it is approved and not running.
    async fn running(&self, core: &Core, manifest: &Arc<Manifest>) -> Result<Arc<Process>> {
        let id = &manifest.id;
        if let Some(process) = self.current(id, &manifest.fingerprint)? {
            return Ok(process);
        }
        let lock = self
            .starting
            .lock()
            .unwrap()
            .entry(id.clone())
            .or_default()
            .clone();
        let _starting = lock.lock().await;
        if let Some(process) = self.current(id, &manifest.fingerprint)? {
            return Ok(process);
        }

        // The program may have changed since the last scan. Check the files
        // that are about to run against the approval.
        let (check, data) = (manifest.clone(), self.data_dir.join(id));
        let make_data = data.clone();
        let (current, made) = tokio::task::spawn_blocking(move || {
            (
                manifest::load(&check.dir),
                std::fs::create_dir_all(&make_data),
            )
        })
        .await
        .map_err(|e| Error::Task(e.to_string()))?;
        made.map_err(|e| Error::io(&data, e))?;
        if current.map(|m| m.fingerprint).as_ref() != Ok(&manifest.fingerprint) {
            // A rescan marks it as needing approval and tells clients.
            let core = core.clone();
            let _ = tokio::task::spawn_blocking(move || core.reload()).await;
            return Err(Error::AddonNotApproved(id.clone()));
        }

        let generation = {
            let mut state = self.state();
            state.next_generation += 1;
            state.next_generation
        };
        let process = match rpc::start(core, manifest.clone(), &data, generation).await {
            Ok(process) => Arc::new(process),
            Err(error) => {
                self.record_failure(id, error.clone());
                core.emit(Event::AddonsChanged {
                    addons: self.infos(),
                });
                return Err(addon_error(manifest, error));
            }
        };
        {
            let mut state = self.state();
            match state.addons.get_mut(id) {
                // Revoked or rescanned meanwhile: dropping `process` stops it.
                Some(addon)
                    if addon.manifest.fingerprint == manifest.fingerprint
                        && approved(&self.approvals, manifest) =>
                {
                    addon.process = Some(process.clone());
                }
                _ => return Err(Error::AddonNotApproved(id.clone())),
            }
        }
        core.emit(Event::AddonsChanged {
            addons: self.infos(),
        });
        Ok(process)
    }

    /// The running process, `None` when it may be started now, or an error
    /// when it may not.
    fn current(&self, id: &str, fingerprint: &str) -> Result<Option<Arc<Process>>> {
        let state = self.state();
        let addon = state
            .addons
            .get(id)
            .filter(|a| a.manifest.fingerprint == fingerprint)
            .ok_or_else(|| Error::AddonNotApproved(id.to_string()))?;
        if !approved(&self.approvals, &addon.manifest) {
            return Err(Error::AddonNotApproved(id.to_string()));
        }
        if let Some(process) = &addon.process {
            return Ok(Some(process.clone()));
        }
        if let Some(failure) = &addon.failure {
            let wait = failure.retry_at.saturating_duration_since(Instant::now());
            if !wait.is_zero() {
                return Err(Error::Addon {
                    id: id.to_string(),
                    message: format!(
                        "{}; trying again in {} s",
                        failure.error,
                        wait.as_secs() + 1
                    ),
                });
            }
        }
        Ok(None)
    }

    /// Called when a process exits or breaks the protocol. Ignored unless
    /// `generation` is the addon's current process.
    pub(crate) fn stopped(&self, core: &Core, id: &str, generation: u64, error: String) {
        let mut state = self.state();
        let Some(addon) = state.addons.get_mut(id) else {
            return;
        };
        if addon.process.as_ref().map(|p| p.generation) != Some(generation) {
            return;
        }
        tracing::warn!(addon = %id, "{error}");
        // Under the same lock, so nobody sees the addon neither running nor
        // failed.
        let ran = addon.process.take().map(|p| p.started.elapsed());
        addon.fail(error, ran);
        drop(state);
        core.emit(Event::AddonsChanged {
            addons: self.infos(),
        });
    }

    fn record_failure(&self, id: &str, error: String) {
        if let Some(addon) = self.state().addons.get_mut(id) {
            addon.fail(error, None);
        }
    }
}

impl Addon {
    /// The delay doubles while the addon keeps stopping within a minute of
    /// starting, and starts over once it ran longer.
    fn fail(&mut self, error: String, ran: Option<Duration>) {
        let backoff = match (&self.failure, ran) {
            (_, Some(ran)) if ran > MAX_BACKOFF => FIRST_BACKOFF,
            (Some(failure), _) => (failure.backoff * 2).min(MAX_BACKOFF),
            (None, _) => FIRST_BACKOFF,
        };
        self.failure = Some(Failure {
            error,
            retry_at: Instant::now() + backoff,
            backoff,
        });
    }
}

impl State {
    fn infos(&self, approvals: &Pinned<String, String>) -> Vec<AddonInfo> {
        self.addons
            .values()
            .map(|addon| {
                let m = &addon.manifest;
                let state = if !approved(approvals, m) {
                    AddonState::NeedsApproval {
                        changed: approvals.contains(&m.id),
                    }
                } else if addon.process.is_some() {
                    AddonState::Running
                } else if let Some(failure) = &addon.failure {
                    AddonState::Failed {
                        error: failure.error.clone(),
                    }
                } else {
                    AddonState::Ready
                };
                AddonInfo {
                    id: m.id.clone(),
                    name: m.name.clone(),
                    version: m.version.clone(),
                    permissions: m.permissions.iter().map(|p| p.to_string()).collect(),
                    kinds: m.kinds.clone(),
                    commands: m.commands.clone(),
                    actions: m.actions.clone(),
                    program: m.program.clone(),
                    fingerprint: m.fingerprint.clone(),
                    state,
                }
            })
            .collect()
    }
}

fn approved(approvals: &Pinned<String, String>, manifest: &Manifest) -> bool {
    approvals.get(&manifest.id).as_ref() == Some(&manifest.fingerprint)
}

fn addon_error(manifest: &Manifest, message: String) -> Error {
    Error::Addon {
        id: manifest.id.clone(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host for an addons folder holding one addon, `probe`.
    fn host_with_addon(dir: &Path) -> Host {
        use std::os::unix::fs::PermissionsExt;
        let folder = dir.join("addons/probe");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("addon.toml"),
            "id = \"probe\"\nname = \"Probe\"\nversion = \"0.1.0\"\napi = \"1\"\n\
             permissions = [\"nodes:read\"]\n\
             [backend]\ncommand = \"probe\"\nprotocol = \"jsonrpc-stdio\"\n",
        )
        .unwrap();
        std::fs::write(folder.join("probe"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(folder.join("probe"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        Host::new(
            dir.join("addons"),
            dir.join("addons.toml"),
            dir.join("data"),
        )
    }

    fn state(host: &Host) -> AddonState {
        host.infos()[0].state.clone()
    }

    /// The file as Burrow wrote it before the stores were shared. Every
    /// tester's approvals are in such a file, so a change in its shape
    /// would stop their addons until they approve them again.
    #[test]
    fn reads_and_writes_approvals_as_burrow_always_has() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("addons.toml");
        let host = host_with_addon(dir.path());
        host.rescan();
        let fingerprint = host.manifest("probe").unwrap().fingerprint.clone();
        let old_file = format!("[approved]\nprobe = \"{fingerprint}\"\n");
        std::fs::write(&path, &old_file).unwrap();
        host.rescan();
        assert_eq!(state(&host), AddonState::Ready);

        host.revoke("probe").unwrap();
        assert_eq!(state(&host), AddonState::NeedsApproval { changed: false });
        host.approve("probe", &fingerprint).unwrap();
        assert_eq!(state(&host), AddonState::Ready);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), old_file);
    }

    /// Unlike the other choices, a broken approvals file is shown to the
    /// user, since it is why their addons stopped.
    #[test]
    fn a_broken_approvals_file_is_a_problem_and_approves_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("addons.toml");
        let host = host_with_addon(dir.path());
        host.rescan();
        let fingerprint = host.manifest("probe").unwrap().fingerprint.clone();
        std::fs::write(&path, format!("approved = [\nprobe = \"{fingerprint}\"\n")).unwrap();
        host.rescan();
        assert!(
            host.problems().iter().any(|p| p.path == path),
            "{:?}",
            host.problems()
        );
        assert_eq!(state(&host), AddonState::NeedsApproval { changed: false });
    }
}
