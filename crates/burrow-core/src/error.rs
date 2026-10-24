use std::path::PathBuf;

use crate::api::NodeId;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("node {0} not found")]
    NodeNotFound(NodeId),

    #[error("node {0} is not a folder")]
    NotAFolder(NodeId),

    #[error("folder {0} is not empty")]
    FolderNotEmpty(NodeId),

    #[error("a folder cannot be moved into itself or one of its subfolders")]
    MoveIntoSelf,

    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },

    #[error("secret store: {0}")]
    Secrets(String),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not encode {path}: {message}")]
    Encode { path: PathBuf, message: String },

    #[error("file watcher: {0}")]
    Watch(String),

    #[error("session {0} not found")]
    SessionNotFound(crate::api::SessionId),

    #[error("addon {0} not found")]
    AddonNotFound(String),

    #[error("addon {0} is not approved")]
    AddonNotApproved(String),

    #[error("addon {id}: {message}")]
    Addon { id: String, message: String },

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("background task failed: {0}")]
    Task(String),
}

impl Error {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> Self {
        Error::Invalid {
            field,
            reason: reason.into(),
        }
    }
}
