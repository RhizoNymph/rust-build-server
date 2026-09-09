//! Local `rbs server` transport with autostart.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::transport::{BoxFuture, Conn, Transport, TransportError, UnixTransport};

/// How long to keep retrying the socket after spawning the server.
pub const AUTOSTART_WAIT: Duration = Duration::from_secs(3);
const RETRY_EVERY: Duration = Duration::from_millis(100);

/// Connects to `socket`; when that fails and `autostart` is set, spawns
/// `<exe> server` detached (own process group, stdio null) and retries for
/// up to [`AUTOSTART_WAIT`].
#[derive(Debug, Clone)]
pub struct LocalTransport {
    pub socket: PathBuf,
    pub autostart: bool,
    /// Binary to spawn as the server (normally `current_exe()`).
    pub exe: PathBuf,
}

impl LocalTransport {
    fn spawn_server(&self) -> Result<(), TransportError> {
        use std::os::unix::process::CommandExt;
        if let Some(dir) = self.socket.parent() {
            std::fs::create_dir_all(dir)?;
        }
        tracing::warn!(exe = %self.exe.display(), socket = %self.socket.display(), "autostarting local rbs server");
        std::process::Command::new(&self.exe)
            .arg("server")
            .arg("--socket")
            .arg(&self.socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|source| TransportError::Connect {
                target: format!("{} server", self.exe.display()),
                source,
            })?;
        Ok(())
    }
}

impl Transport for LocalTransport {
    fn connect(&self) -> BoxFuture<'_, Result<Box<dyn Conn>, TransportError>> {
        Box::pin(async move {
            let unix = UnixTransport {
                path: self.socket.clone(),
            };
            let first = unix.connect().await;
            if first.is_ok() || !self.autostart {
                return first;
            }
            let first_err = first.err().map(|e| e.to_string()).unwrap_or_default();
            tracing::debug!(error = %first_err, "local server not reachable; autostarting");
            self.spawn_server()?;
            let deadline = Instant::now() + AUTOSTART_WAIT;
            loop {
                tokio::time::sleep(RETRY_EVERY).await;
                match unix.connect().await {
                    Ok(c) => return Ok(c),
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => continue,
                }
            }
        })
    }
    fn describe(&self) -> String {
        self.socket.display().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_autostart_fails_fast_when_socket_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let t = LocalTransport {
            socket: dir.path().join("missing.sock"),
            autostart: false,
            exe: PathBuf::from("/nonexistent/rbs"),
        };
        let t0 = Instant::now();
        let err = t.connect().await.err().expect("must fail");
        assert!(matches!(err, TransportError::Connect { .. }));
        assert!(t0.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn autostart_spawn_failure_is_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let t = LocalTransport {
            socket: dir.path().join("missing.sock"),
            autostart: true,
            exe: PathBuf::from("/nonexistent/rbs-binary"),
        };
        let err = t.connect().await.err().expect("must fail");
        assert!(err.to_string().contains("rbs-binary"));
    }

    #[tokio::test]
    async fn autostart_connects_once_the_server_script_listens() {
        // A fake "server" that creates the socket after a short delay.
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("python3 not available; skipping");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let exe = dir.path().join("fake-rbs");
        std::fs::write(
            &exe,
            "#!/bin/sh\n# $1=server $2=--socket $3=path\nsleep 0.3\nexec python3 -c \"import socket,sys,time; s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1]); s.listen(1); time.sleep(5)\" \"$3\"\n",
        )
        .expect("write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let t = LocalTransport {
            socket: sock.clone(),
            autostart: true,
            exe,
        };
        let res = t.connect().await;
        assert!(
            res.is_ok(),
            "expected connect via autostart: {:?}",
            res.err()
        );
    }
}
