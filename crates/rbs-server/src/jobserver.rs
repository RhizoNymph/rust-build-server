//! GNU jobserver token pool backed by a named FIFO.
//!
//! cargo (and make) inherit the pool through `MAKEFLAGS=--jobserver-auth=fifo:<path>`
//! so every compile across every job on the host competes for the same `N`
//! slots regardless of `-j`.
//!
//! `tokens_free` is *best effort*: rather than probing the FIFO with an ioctl
//! (which needs `unsafe`), it is derived as `tokens_total - running_jobs`
//! clamped at zero. It is informational only; admission never depends on it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::sys::stat::Mode;
use tracing::{debug, info};

use crate::error::ServerError;

/// File name of the FIFO inside the server's state directory.
pub const FIFO_NAME: &str = "jobserver.fifo";

/// An open jobserver pool. Dropping it closes the FIFO and unlinks it.
#[derive(Debug)]
pub struct JobServer {
    path: PathBuf,
    total: u32,
    /// Read+write handle; keeps the FIFO alive even while no job holds it.
    _keepalive: File,
}

impl JobServer {
    /// Create `<dir>/jobserver.fifo`, open it read+write and pre-fill `tokens` slots.
    pub fn create(dir: &Path, tokens: u32) -> Result<JobServer, ServerError> {
        let path = dir.join(FIFO_NAME);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ServerError::RemoveStale {
                    path: path.clone(),
                    source,
                });
            }
        }
        nix::unistd::mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).map_err(|source| {
            ServerError::Mkfifo {
                path: path.clone(),
                source,
            }
        })?;
        // O_RDWR on a FIFO never blocks on Linux and keeps both ends open.
        let mut keepalive = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| ServerError::OpenFifo {
                path: path.clone(),
                source,
            })?;
        let fill = vec![b'+'; tokens as usize];
        keepalive
            .write_all(&fill)
            .map_err(|source| ServerError::FillFifo {
                path: path.clone(),
                source,
            })?;
        info!(path = %path.display(), tokens, "jobserver pool created");
        Ok(JobServer {
            path,
            total: tokens,
            _keepalive: keepalive,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn tokens_total(&self) -> u32 {
        self.total
    }

    /// Best-effort free token count; see module docs.
    pub fn tokens_free(&self, running_jobs: usize) -> u32 {
        let running = u32::try_from(running_jobs).unwrap_or(u32::MAX);
        self.total.saturating_sub(running)
    }

    /// Value for `MAKEFLAGS` / `CARGO_MAKEFLAGS`.
    pub fn makeflags(&self) -> String {
        format!("--jobserver-auth=fifo:{}", self.path.display())
    }
}

impl Drop for JobServer {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            debug!(path = %self.path.display(), error = %e, "could not unlink jobserver fifo");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn creates_fifo_and_prefills_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let js = JobServer::create(dir.path(), 5).expect("create");
        assert_eq!(js.tokens_total(), 5);
        assert_eq!(js.tokens_free(0), 5);
        assert_eq!(js.tokens_free(2), 3);
        assert_eq!(js.tokens_free(99), 0);
        assert!(js.makeflags().starts_with("--jobserver-auth=fifo:"));
        assert!(js.makeflags().ends_with("jobserver.fifo"));

        let meta = std::fs::metadata(js.path()).expect("metadata");
        assert!(std::os::unix::fs::FileTypeExt::is_fifo(&meta.file_type()));

        // Read the tokens back: exactly 5 `+` bytes are available.
        let mut reader = OpenOptions::new()
            .read(true)
            .open(js.path())
            .expect("open read end");
        let mut buf = [0u8; 5];
        reader.read_exact(&mut buf).expect("read tokens");
        assert_eq!(&buf, b"+++++");

        let path = js.path().to_path_buf();
        drop(reader);
        drop(js);
        assert!(!path.exists(), "fifo removed on drop");
    }

    #[test]
    fn replaces_stale_fifo() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(FIFO_NAME), b"stale").expect("write stale");
        let js = JobServer::create(dir.path(), 1).expect("create over stale");
        assert_eq!(js.tokens_total(), 1);
    }

    #[test]
    fn zero_tokens_is_allowed_but_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let js = JobServer::create(dir.path(), 0).expect("create");
        assert_eq!(js.tokens_free(0), 0);
    }
}
