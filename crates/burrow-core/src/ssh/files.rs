//! SFTP sessions for file panels: listing and changing remote folders, and
//! downloads and uploads that report progress and can be cancelled.
//!
//! The server is not trusted to send sensible names. A name holding `/`, or
//! one that is `.` or `..`, would make a recursive download write outside
//! the folder the user picked, so such names fail the transfer. Symlinks
//! found inside a folder are skipped rather than followed, on both sides.
//!
//! File bytes never go through events. A transfer copies straight between
//! the SFTP channel and the local file, and reports progress as
//! [`Event::TransferChanged`] a few times a second.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use super::{Input, Prompter};
use crate::api::{
    Event, FileEntry, FileKind, Node, SessionId, SessionState, TransferId, TransferState,
};
use crate::engine::Core;

type Result<T> = std::result::Result<T, String>;

/// How long the server gets to answer each SFTP request.
const REQUEST_TIMEOUT: u64 = 30;

/// Bytes copied per read.
const CHUNK: usize = 256 * 1024;

/// Copies are written private, and only given their real mode once
/// complete.
const TEMP_MODE: u32 = 0o600;

/// The mode a copy gets. One that replaces a file keeps that file's mode,
/// so a private file stays private. A new one takes its source's, but never
/// writable by group or other, and never set-id.
fn copy_mode(replaced: Option<u32>, source: Option<u32>) -> u32 {
    match replaced {
        Some(mode) => mode & 0o777,
        None => source.map_or(0o644, |mode| mode & 0o755),
    }
}

/// The least time between two progress events for one transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// Opens the SFTP subsystem, then waits until the session is closed or its
/// connection ends. Requests use the [`SftpSession`] it registers.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    mut input: mpsc::UnboundedReceiver<Input>,
    prompter: &Prompter,
) -> Result<()> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let setup = async {
        let connection = core.connections().get(core, node, prompter, &step).await?;
        step("Starting SFTP".into());
        let channel = connection
            .handle()
            .channel_open_session()
            .await
            .map_err(text)?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .map_err(text)?;
        let config = russh_sftp::client::Config {
            request_timeout_secs: REQUEST_TIMEOUT,
            ..Default::default()
        };
        let sftp = SftpSession::new_with_config(channel.into_stream(), config)
            .await
            .map_err(|e| format!("the server did not start SFTP: {e}"))?;
        Ok::<_, String>((connection, sftp))
    };
    let closed = async {
        while let Some(input) = input.recv().await {
            if let Input::Close = input {
                return;
            }
        }
    };
    let (connection, sftp) = tokio::select! {
        result = setup => result?,
        () = closed => return Ok(()),
    };
    core.sessions().set_sftp(prompter.session, Arc::new(sftp));
    core.emit(Event::SessionChanged {
        session: prompter.session,
        node: node.id,
        state: SessionState::Connected,
    });
    loop {
        tokio::select! {
            reason = connection.closed() => return Err(reason),
            message = input.recv() => match message {
                Some(Input::Close) | None => return Ok(()),
                Some(_) => {}
            },
        }
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// `name` inside the remote folder `dir`.
pub(crate) fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// A name from a folder listing that is safe to join onto a path.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        Err(format!("the server sent an unsafe file name, {name:?}"))
    } else {
        Ok(())
    }
}

fn kind(file_type: FileType) -> FileKind {
    match file_type {
        FileType::Dir => FileKind::Dir,
        FileType::File => FileKind::File,
        FileType::Symlink => FileKind::Symlink,
        FileType::Other => FileKind::Other,
    }
}

/// The folder's absolute path and its entries, sorted by name. Entries
/// with unsafe names are left out.
pub(crate) async fn list_dir(sftp: &SftpSession, path: &str) -> Result<(String, Vec<FileEntry>)> {
    let path = sftp
        .canonicalize(if path.is_empty() { "." } else { path })
        .await
        .map_err(text)?;
    let mut entries: Vec<FileEntry> = sftp
        .read_dir(&path)
        .await
        .map_err(text)?
        .filter(|entry| check_name(&entry.file_name()).is_ok())
        .map(|entry| {
            let metadata = entry.metadata();
            FileEntry {
                name: entry.file_name(),
                kind: kind(entry.file_type()),
                size: metadata.size.unwrap_or(0),
                modified: metadata.mtime.map(u64::from),
                permissions: metadata.permissions.map(|p| p & 0o7777),
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((path, entries))
}

pub(crate) async fn make_dir(sftp: &SftpSession, path: &str) -> Result<()> {
    sftp.create_dir(path).await.map_err(text)
}

pub(crate) async fn rename(sftp: &SftpSession, from: &str, to: &str) -> Result<()> {
    if sftp.try_exists(to).await.map_err(text)? {
        return Err(format!("{to} already exists"));
    }
    sftp.rename(from, to).await.map_err(text)
}

/// Deletes a file, a symlink (not what it points at), or a folder and
/// everything in it.
pub(crate) async fn remove(sftp: &SftpSession, path: &str) -> Result<()> {
    if path.trim_end_matches('/').is_empty() {
        return Err("Burrow won't delete the root folder".into());
    }
    let metadata = sftp.symlink_metadata(path).await.map_err(text)?;
    if metadata.file_type() != FileType::Dir {
        return sftp.remove_file(path).await.map_err(text);
    }
    for entry in sftp.read_dir(path).await.map_err(text)? {
        let name = entry.file_name();
        check_name(&name)?;
        Box::pin(remove(sftp, &join(path, &name))).await?;
    }
    sftp.remove_dir(path).await.map_err(text)
}

/// Running transfers, so they can be cancelled.
#[derive(Default)]
pub(crate) struct Transfers {
    next: AtomicU64,
    map: Mutex<HashMap<TransferId, Running>>,
}

struct Running {
    session: SessionId,
    cancel: watch::Sender<bool>,
}

impl Transfers {
    pub fn cancel(&self, id: TransferId) -> crate::error::Result<()> {
        let map = self.map.lock().unwrap();
        let running = map.get(&id).ok_or_else(|| {
            crate::error::Error::invalid("transfer", format!("{id} is not running"))
        })?;
        running.cancel.send_replace(true);
        Ok(())
    }

    /// Cancels every transfer of a session that ended.
    pub fn cancel_session(&self, session: SessionId) {
        for running in self.map.lock().unwrap().values() {
            if running.session == session {
                running.cancel.send_replace(true);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Direction {
    Download { remote: String, local: PathBuf },
    Upload { local: PathBuf, remote: String },
}

/// Starts a transfer in the background and returns its id at once.
pub(crate) fn start(
    core: &Core,
    session: SessionId,
    sftp: Arc<SftpSession>,
    direction: Direction,
    overwrite: bool,
) -> TransferId {
    let transfers = core.transfers();
    let id = TransferId(transfers.next.fetch_add(1, Ordering::Relaxed) + 1);
    let (cancel, mut cancelled) = watch::channel(false);
    transfers
        .map
        .lock()
        .unwrap()
        .insert(id, Running { session, cancel });
    let core = core.clone();
    tokio::spawn(async move {
        let mut progress = Progress {
            core: core.clone(),
            transfer: id,
            session,
            done: 0,
            total: 0,
            last: None,
        };
        let partial = Partial::default();
        let work = async {
            match &direction {
                Direction::Download { remote, local } => {
                    download(&sftp, remote, local, overwrite, &mut progress, &partial).await
                }
                Direction::Upload { local, remote } => {
                    upload(&sftp, local, remote, overwrite, &mut progress, &partial).await
                }
            }
        };
        let state = tokio::select! {
            result = work => match result {
                Ok(()) => TransferState::Finished,
                Err(error) => TransferState::Failed { error },
            },
            _ = cancelled.wait_for(|&c| c) => TransferState::Cancelled,
        };
        partial.clean_up(&sftp).await;
        core.transfers().map.lock().unwrap().remove(&id);
        core.emit(Event::TransferChanged {
            transfer: id,
            session,
            state,
        });
    });
    id
}

struct Progress {
    core: Core,
    transfer: TransferId,
    session: SessionId,
    done: u64,
    total: u64,
    last: Option<Instant>,
}

impl Progress {
    fn add(&mut self, bytes: u64) {
        self.done += bytes;
        if self
            .last
            .is_none_or(|last| last.elapsed() >= PROGRESS_INTERVAL)
        {
            self.last = Some(Instant::now());
            self.core.emit(Event::TransferChanged {
                transfer: self.transfer,
                session: self.session,
                state: TransferState::Running {
                    done: self.done,
                    total: self.total,
                },
            });
        }
    }
}

/// The file being written, removed if the transfer stops part way.
#[derive(Default)]
struct Partial(Mutex<Option<Target>>);

enum Target {
    Local(PathBuf),
    Remote(String),
}

impl Partial {
    fn set(&self, target: Option<Target>) {
        *self.0.lock().unwrap() = target;
    }

    async fn clean_up(&self, sftp: &SftpSession) {
        let target = self.0.lock().unwrap().take();
        match target {
            Some(Target::Local(path)) => {
                let _ = tokio::fs::remove_file(path).await;
            }
            Some(Target::Remote(path)) => {
                let _ = sftp.remove_file(path).await;
            }
            None => {}
        }
    }
}

/// What a transfer copies, worked out before any byte moves so progress
/// has a total.
enum Item<L, R> {
    /// A folder to create, unless it is there already.
    Dir {
        to: R,
    },
    File {
        from: L,
        to: R,
        size: u64,
    },
}

async fn download(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    overwrite: bool,
    progress: &mut Progress,
    partial: &Partial,
) -> Result<()> {
    if !local.is_absolute() {
        return Err(format!("{} is not an absolute path", local.display()));
    }
    // The item the user picked is followed if it is a symlink; anything
    // found inside a folder is not.
    let metadata = sftp.metadata(remote).await.map_err(text)?;
    let is_dir = match metadata.file_type() {
        FileType::Dir => true,
        FileType::File => false,
        _ => return Err(format!("{remote} is not a file or folder")),
    };
    match tokio::fs::symlink_metadata(local).await {
        Ok(_) if !overwrite => return Err(format!("{} already exists", local.display())),
        Ok(existing) if existing.is_symlink() => {
            return Err(format!(
                "{} is a symlink, which Burrow won't write through",
                local.display()
            ));
        }
        Ok(existing) if existing.is_dir() != is_dir => {
            return Err(format!(
                "{} already exists and is not a {}",
                local.display(),
                if is_dir { "folder" } else { "file" }
            ));
        }
        _ => {}
    }

    let mut items = Vec::new();
    if is_dir {
        plan_download(sftp, remote.to_string(), local.to_path_buf(), &mut items).await?;
    } else {
        items.push(Item::File {
            from: remote.to_string(),
            to: local.to_path_buf(),
            size: metadata.size.unwrap_or(0),
        });
    }
    progress.total = items
        .iter()
        .map(|item| match item {
            Item::File { size, .. } => *size,
            Item::Dir { .. } => 0,
        })
        .sum();
    progress.add(0);

    for item in items {
        match item {
            Item::Dir { to } => match tokio::fs::symlink_metadata(&to).await {
                Ok(existing) if existing.is_dir() => {}
                Ok(_) => return Err(format!("{} is in the way of a folder", to.display())),
                Err(_) => tokio::fs::create_dir(&to)
                    .await
                    .map_err(|e| format!("{}: {e}", to.display()))?,
            },
            Item::File { from, to, .. } => {
                download_file(sftp, &from, &to, progress, partial).await?;
            }
        }
    }
    Ok(())
}

/// Lists a remote folder's contents, folders before what is in them.
async fn plan_download(
    sftp: &SftpSession,
    remote: String,
    local: PathBuf,
    items: &mut Vec<Item<String, PathBuf>>,
) -> Result<()> {
    let entries = sftp.read_dir(&remote).await.map_err(text)?;
    items.push(Item::Dir { to: local.clone() });
    for entry in entries {
        let name = entry.file_name();
        check_name(&name)?;
        let (from, to) = (join(&remote, &name), local.join(&name));
        match entry.file_type() {
            FileType::Dir => Box::pin(plan_download(sftp, from, to, items)).await?,
            FileType::File => items.push(Item::File {
                from,
                to,
                size: entry.metadata().size.unwrap_or(0),
            }),
            FileType::Symlink | FileType::Other => {}
        }
    }
    Ok(())
}

/// Copies into a hidden file next to the target, then renames it into
/// place, so a stopped download never leaves half a file under the real
/// name.
async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    progress: &mut Progress,
    partial: &Partial,
) -> Result<()> {
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = local.with_file_name(format!(".{name}.burrow-{}", progress.transfer));
    partial.set(Some(Target::Local(temp.clone())));
    let local_error = |e: std::io::Error| format!("{}: {e}", local.display());

    let mut from = sftp.open(remote).await.map_err(text)?;
    let source = from.metadata().await.ok().and_then(|m| m.permissions);
    let replaced = tokio::fs::symlink_metadata(local)
        .await
        .ok()
        .filter(|m| m.is_file())
        .map(|m| m.permissions().mode());
    let mut to = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(TEMP_MODE)
        .open(&temp)
        .await
        .map_err(local_error)?;
    let mut buffer = vec![0; CHUNK];
    loop {
        let read = from.read(&mut buffer).await.map_err(text)?;
        if read == 0 {
            break;
        }
        to.write_all(&buffer[..read]).await.map_err(local_error)?;
        progress.add(read as u64);
    }
    to.flush().await.map_err(local_error)?;
    let mode = std::fs::Permissions::from_mode(copy_mode(replaced, source));
    to.set_permissions(mode).await.map_err(local_error)?;
    drop(to);
    tokio::fs::rename(&temp, local).await.map_err(local_error)?;
    partial.set(None);
    Ok(())
}

async fn upload(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    overwrite: bool,
    progress: &mut Progress,
    partial: &Partial,
) -> Result<()> {
    if !local.is_absolute() {
        return Err(format!("{} is not an absolute path", local.display()));
    }
    let local_error = |e: std::io::Error| format!("{}: {e}", local.display());
    let metadata = tokio::fs::metadata(local).await.map_err(local_error)?;
    if !metadata.is_dir() && !metadata.is_file() {
        return Err(format!("{} is not a file or folder", local.display()));
    }
    match sftp.symlink_metadata(remote).await {
        Ok(_) if !overwrite => return Err(format!("{remote} already exists")),
        Ok(existing) if existing.file_type() == FileType::Symlink => {
            return Err(format!(
                "{remote} is a symlink, which Burrow won't write through"
            ));
        }
        Ok(existing) if existing.is_dir() != metadata.is_dir() => {
            return Err(format!(
                "{remote} already exists and is not a {}",
                if metadata.is_dir() { "folder" } else { "file" }
            ));
        }
        _ => {}
    }

    let mut items = Vec::new();
    if metadata.is_dir() {
        plan_upload(local.to_path_buf(), remote.to_string(), &mut items).await?;
    } else {
        items.push(Item::File {
            from: local.to_path_buf(),
            to: remote.to_string(),
            size: metadata.len(),
        });
    }
    progress.total = items
        .iter()
        .map(|item| match item {
            Item::File { size, .. } => *size,
            Item::Dir { .. } => 0,
        })
        .sum();
    progress.add(0);

    for item in items {
        match item {
            Item::Dir { to } => match sftp.symlink_metadata(&to).await {
                Ok(existing) if existing.is_dir() => {}
                Ok(_) => return Err(format!("{to} is in the way of a folder")),
                Err(_) => sftp.create_dir(&to).await.map_err(text)?,
            },
            Item::File { from, to, .. } => {
                upload_file(sftp, &from, &to, progress, partial).await?;
            }
        }
    }
    Ok(())
}

/// Lists a local folder's contents, folders before what is in them.
async fn plan_upload(
    local: PathBuf,
    remote: String,
    items: &mut Vec<Item<PathBuf, String>>,
) -> Result<()> {
    let local_error = |e: std::io::Error| format!("{}: {e}", local.display());
    let mut entries = tokio::fs::read_dir(&local).await.map_err(local_error)?;
    items.push(Item::Dir { to: remote.clone() });
    while let Some(entry) = entries.next_entry().await.map_err(local_error)? {
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", entry.path().display()))?;
        let (from, to) = (entry.path(), join(&remote, name));
        let file_type = entry.file_type().await.map_err(local_error)?;
        if file_type.is_dir() {
            Box::pin(plan_upload(from, to, items)).await?;
        } else if file_type.is_file() {
            let size = entry.metadata().await.map_err(local_error)?.len();
            items.push(Item::File { from, to, size });
        }
    }
    Ok(())
}

/// Copies into a new hidden file next to the target, then moves it into
/// place. A stopped upload leaves a file it replaces as it was, and the
/// target is never opened, so a symlink there is never written through.
async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    progress: &mut Progress,
    partial: &Partial,
) -> Result<()> {
    let (dir, name) = remote.rsplit_once('/').unwrap_or((".", remote));
    let temp = join(
        if dir.is_empty() { "/" } else { dir },
        &format!(".{name}.burrow-{}", progress.transfer),
    );
    let local_error = |e: std::io::Error| format!("{}: {e}", local.display());
    let mut from = tokio::fs::File::open(local).await.map_err(local_error)?;
    let source = from.metadata().await.ok().map(|m| m.permissions().mode());
    // EXCLUDE: fail rather than follow anything already at that name.
    let flags = OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE;
    let mut to = sftp
        .open_with_flags_and_attributes(&temp, flags, mode_attributes(TEMP_MODE))
        .await
        .map_err(text)?;
    partial.set(Some(Target::Remote(temp.clone())));
    let mut buffer = vec![0; CHUNK];
    loop {
        let read = from.read(&mut buffer).await.map_err(local_error)?;
        if read == 0 {
            break;
        }
        to.write_all(&buffer[..read]).await.map_err(text)?;
        progress.add(read as u64);
    }

    // SFTP's rename won't replace a file, so the old one goes first. The
    // mode is set through the open handle, so it can only reach our file.
    let existing = sftp.symlink_metadata(remote).await;
    let replaced = match &existing {
        Ok(existing) if existing.file_type() == FileType::File => existing.permissions,
        _ => None,
    };
    let mode = copy_mode(replaced, source);
    to.set_metadata(mode_attributes(mode)).await.map_err(text)?;
    to.shutdown().await.map_err(text)?;
    match existing {
        Ok(existing) if existing.file_type() == FileType::File => {
            sftp.remove_file(remote).await.map_err(text)?;
        }
        Ok(existing) if existing.file_type() == FileType::Symlink => {
            return Err(format!(
                "{remote} is a symlink, which Burrow won't write through"
            ));
        }
        Ok(_) => return Err(format!("{remote} is in the way of a file")),
        Err(_) => {}
    }
    sftp.rename(&temp, remote).await.map_err(text)?;
    partial.set(None);
    Ok(())
}

/// Attributes that set only the permission bits.
fn mode_attributes(mode: u32) -> FileAttributes {
    FileAttributes {
        permissions: Some(mode),
        ..FileAttributes::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_keep_a_replaced_mode_or_drop_shared_writes() {
        assert_eq!(copy_mode(Some(0o100600), Some(0o644)), 0o600);
        assert_eq!(copy_mode(None, Some(0o100666)), 0o644);
        assert_eq!(copy_mode(None, Some(0o104755)), 0o755);
        assert_eq!(copy_mode(None, None), 0o644);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for name in ["", ".", "..", "a/b", "/", "a\0b"] {
            assert!(check_name(name).is_err(), "{name:?}");
        }
        for name in ["a", "...", ".hidden", "with space", "a\\b"] {
            assert!(check_name(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn joins_remote_paths() {
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("/home/me", "x"), "/home/me/x");
        assert_eq!(join("/home/me/", "x"), "/home/me/x");
    }
}
