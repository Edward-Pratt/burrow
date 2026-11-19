use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Why a [`Client`](crate::Client) call failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The daemon's socket couldn't be reached.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The daemon refused or failed the request, as its error says, or
    /// the connection to it is gone.
    #[error("{0}")]
    Remote(String),
}
