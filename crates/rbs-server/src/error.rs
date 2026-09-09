//! Structured error type for the server crate. `anyhow` is used only at the
//! `run_*` boundary in `lib.rs`.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("failed to create directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to remove stale {path}: {source}")]
    RemoveStale {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to bind unix socket {path}: {source}")]
    Bind {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to set permissions on {path}: {source}")]
    Chmod {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create jobserver fifo {path}: {source}")]
    Mkfifo {
        path: PathBuf,
        #[source]
        source: nix::Error,
    },
    #[error("failed to open jobserver fifo {path}: {source}")]
    OpenFifo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to fill jobserver fifo {path}: {source}")]
    FillFifo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("socket path {0} has no parent directory")]
    NoParent(PathBuf),
    #[error("connection I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol framing error: {0}")]
    Frame(#[from] rbs_proto::FrameError),
    #[error("failed to connect to {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to install signal handler: {0}")]
    Signal(#[source] std::io::Error),
}
