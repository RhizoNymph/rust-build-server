//! Job execution: toolchain verification, process spawn (optionally inside a
//! transient systemd scope), output pumping, cancellation and timeouts.

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use rbs_proto::{ExitStatus, JobEvent, JobId, JobRequest, RejectReason};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// How long to wait after SIGTERM before SIGKILL.
pub const KILL_GRACE: Duration = Duration::from_secs(5);
/// Output pump read size; each chunk becomes one `Stdout`/`Stderr` event.
const CHUNK: usize = 16 * 1024;
/// After the process exits, how long to keep draining pipes held open by
/// stray grandchildren before giving up.
const DRAIN_GRACE: Duration = Duration::from_secs(10);

/// Environment variables inherited from the server process into every job.
pub const INHERITED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TERM",
    "LANG",
    // `systemd-run --user` needs the user bus; without these it fails with
    // "Failed to connect to bus" and every scoped job would exit 1.
    "XDG_RUNTIME_DIR",
    "DBUS_SESSION_BUS_ADDRESS",
];

/// Apply [`INHERITED_ENV`] from the server's environment onto a command
/// (used for both jobs and the systemd scope probe, so the probe sees exactly
/// what jobs will see).
fn inherit_env(cmd: &mut Command) {
    cmd.env_clear();
    for key in INHERITED_ENV {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
}

/// cgroup scope wrapping policy for spawned jobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Run the process directly.
    None,
    /// Wrap in `systemd-run --user --scope` with the given `MemoryMax` (GiB).
    Systemd { mem_max_gib: u32 },
}

/// Spawns and supervises jobs. One per server.
#[derive(Debug)]
pub struct Runner {
    /// Value of `MAKEFLAGS` / `CARGO_MAKEFLAGS` for jobs.
    pub makeflags: String,
    pub scope: Scope,
    /// `None` = no timeout.
    pub timeout: Option<Duration>,
    /// This host's name, used in toolchain mismatch reports.
    pub hostname: String,
    scope_warned: AtomicBool,
}

/// A job that has been spawned. Dropping the handle cancels the job.
#[derive(Debug)]
pub struct JobHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl JobHandle {
    /// Ask the job to stop (SIGTERM the group, SIGKILL after [`KILL_GRACE`]).
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }

    /// Split into the cancel trigger and the completion task.
    pub fn into_parts(mut self) -> (CancelToken, JoinHandle<()>) {
        let cancel = self.cancel.take();
        let task = std::mem::replace(&mut self.task, tokio::spawn(async {}));
        (CancelToken { cancel }, task)
    }
}

impl Drop for JobHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Cancel trigger detached from the completion task. Dropping it cancels.
#[derive(Debug)]
pub struct CancelToken {
    cancel: Option<oneshot::Sender<()>>,
}

impl CancelToken {
    pub fn cancel(&mut self) {
        if let Some(tx) = self.cancel.take() {
            let _ = tx.send(());
        }
    }
}

impl Drop for CancelToken {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Runner {
    pub fn new(
        makeflags: String,
        scope: Scope,
        timeout: Option<Duration>,
        hostname: String,
    ) -> Runner {
        Runner {
            makeflags,
            scope,
            timeout,
            hostname,
            scope_warned: AtomicBool::new(false),
        }
    }

    /// Validate `cwd` (absolute, existing directory).
    pub fn validate_cwd(cwd: &Path) -> Result<(), Box<RejectReason>> {
        if !cwd.is_absolute() {
            return Err(Box::new(RejectReason::Internal(format!(
                "cwd {} is not absolute",
                cwd.display()
            ))));
        }
        if !cwd.is_dir() {
            return Err(Box::new(RejectReason::Internal(format!(
                "cwd {} does not exist on the server",
                cwd.display()
            ))));
        }
        Ok(())
    }

    /// Fingerprint this host's toolchain for `req.cwd` and compare with the client's.
    pub async fn check_toolchain(&self, req: &JobRequest) -> Result<(), Box<RejectReason>> {
        let cwd = req.cwd.clone();
        let server_fp = tokio::task::spawn_blocking(move || rbs_toolchain::fingerprint(&cwd))
            .await
            .map_err(|e| {
                Box::new(RejectReason::Internal(format!(
                    "fingerprint task failed: {e}"
                )))
            })?
            .map_err(|e| {
                Box::new(RejectReason::Internal(format!(
                    "toolchain fingerprint failed: {e}"
                )))
            })?;
        match rbs_toolchain::check(
            &req.client.hostname,
            &req.toolchain,
            &self.hostname,
            &server_fp,
        ) {
            Ok(()) => Ok(()),
            Err(m) => {
                warn!(
                    client_host = %m.client_host,
                    client = %m.client,
                    server = %m.server,
                    "toolchain mismatch"
                );
                Err(Box::new(RejectReason::ToolchainMismatch {
                    server: m.server,
                    client: m.client,
                }))
            }
        }
    }

    fn build_command(&self, req: &JobRequest, scope: Scope) -> Command {
        let mut cmd = match scope {
            Scope::Systemd { mem_max_gib } => {
                let mut c = Command::new("systemd-run");
                c.args(["--user", "--scope", "--quiet"])
                    .arg("-p")
                    .arg(format!("MemoryMax={mem_max_gib}G"))
                    .arg("-p")
                    .arg(format!("CPUWeight={}", req.priority.cpu_weight()))
                    .arg("--")
                    .args(&req.argv);
                c
            }
            Scope::None => {
                let mut c = Command::new(&req.argv[0]);
                c.args(&req.argv[1..]);
                c
            }
        };
        cmd.current_dir(&req.cwd).env_clear();
        inherit_env(&mut cmd);
        cmd.envs(&req.env)
            .env("MAKEFLAGS", &self.makeflags)
            .env("CARGO_MAKEFLAGS", &self.makeflags)
            .env("RUSTC_WRAPPER", "kache")
            .env("RBS_SHIM_ACTIVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    fn spawn_child(&self, req: &JobRequest) -> std::io::Result<Child> {
        match self.scope {
            Scope::None => self.build_command(req, Scope::None).spawn(),
            scoped @ Scope::Systemd { .. } => match self.build_command(req, scoped).spawn() {
                Ok(child) => Ok(child),
                Err(e) => {
                    if !self.scope_warned.swap(true, Ordering::Relaxed) {
                        warn!(error = %e, "systemd-run failed to spawn; running jobs unscoped");
                    }
                    self.build_command(req, Scope::None).spawn()
                }
            },
        }
    }

    /// Spawn the job. `Started` is emitted on `events` once the process exists;
    /// the returned handle controls cancellation. Errors are reported as a
    /// reject reason (the caller sends `Rejected`).
    pub fn spawn(
        &self,
        id: JobId,
        req: &JobRequest,
        events: mpsc::Sender<JobEvent>,
    ) -> Result<JobHandle, Box<RejectReason>> {
        if req.argv.is_empty() {
            return Err(Box::new(RejectReason::Internal("empty argv".into())));
        }
        let child = self.spawn_child(req).map_err(|e| {
            Box::new(RejectReason::Internal(format!(
                "spawn {:?} failed: {e}",
                req.argv[0]
            )))
        })?;
        info!(
            job = id.0,
            pid = child.id().unwrap_or(0),
            argv = ?req.argv,
            cwd = %req.cwd.display(),
            client = %req.client.hostname,
            label = req.client.label.as_deref().unwrap_or(""),
            "job started"
        );
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let timeout = self.timeout;
        let task = tokio::spawn(supervise(id, child, events, cancel_rx, timeout));
        Ok(JobHandle {
            cancel: Some(cancel_tx),
            task,
        })
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    events: mpsc::Sender<JobEvent>,
    make: impl Fn(Vec<u8>) -> JobEvent,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                // A closed receiver means the client is gone; keep draining so
                // the child never blocks on a full pipe.
                let _ = events.send(make(buf[..n].to_vec())).await;
            }
        }
    }
}

fn status_of(st: std::process::ExitStatus) -> ExitStatus {
    match (st.code(), st.signal()) {
        (Some(c), _) => ExitStatus::Code(c),
        (None, Some(s)) => ExitStatus::Signal(s),
        (None, None) => ExitStatus::Code(-1),
    }
}

fn signal_group(pid: u32, sig: Signal) {
    let Ok(raw) = i32::try_from(pid) else {
        return;
    };
    if let Err(e) = killpg(Pid::from_raw(raw), sig) {
        debug!(pid, signal = ?sig, error = %e, "killpg failed");
    }
}

/// Terminate the group: SIGTERM, then SIGKILL after [`KILL_GRACE`].
async fn terminate(child: &mut Child, pid: u32) -> std::process::ExitStatus {
    signal_group(pid, Signal::SIGTERM);
    match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(Ok(st)) => return st,
        Ok(Err(e)) => debug!(pid, error = %e, "wait after SIGTERM failed"),
        Err(_) => debug!(pid, "job ignored SIGTERM; sending SIGKILL"),
    }
    signal_group(pid, Signal::SIGKILL);
    match child.wait().await {
        Ok(st) => st,
        Err(e) => {
            debug!(pid, error = %e, "wait after SIGKILL failed");
            std::process::ExitStatus::from_raw(9)
        }
    }
}

async fn supervise(
    id: JobId,
    mut child: Child,
    events: mpsc::Sender<JobEvent>,
    cancel: oneshot::Receiver<()>,
    timeout: Option<Duration>,
) {
    let pid = child.id().unwrap_or(0);
    let _ = events.send(JobEvent::Started { id }).await;
    let out_pump = child.stdout.take().map(|r| {
        tokio::spawn(pump(r, events.clone(), move |bytes| JobEvent::Stdout {
            id,
            bytes,
        }))
    });
    let err_pump = child.stderr.take().map(|r| {
        tokio::spawn(pump(r, events.clone(), move |bytes| JobEvent::Stderr {
            id,
            bytes,
        }))
    });

    let deadline = async {
        match timeout {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    let status = tokio::select! {
        res = child.wait() => match res {
            Ok(st) => st,
            Err(e) => {
                warn!(job = id.0, pid, error = %e, "wait failed");
                std::process::ExitStatus::from_raw(1 << 8)
            }
        },
        _ = cancel => {
            info!(job = id.0, pid, "job cancelled");
            terminate(&mut child, pid).await
        }
        _ = deadline => {
            warn!(job = id.0, pid, timeout_secs = timeout.map(|d| d.as_secs()).unwrap_or(0), "job timed out");
            terminate(&mut child, pid).await
        }
    };
    // Make sure nothing in the group outlives the job (e.g. a stray rustc).
    signal_group(pid, Signal::SIGKILL);
    for p in [out_pump, err_pump].into_iter().flatten() {
        if tokio::time::timeout(DRAIN_GRACE, &mut { p }).await.is_err() {
            debug!(job = id.0, "output pump did not finish; dropping");
        }
    }
    let status = status_of(status);
    info!(job = id.0, pid, status = ?status, "job exited");
    let _ = events.send(JobEvent::Exited { id, status }).await;
}

/// Check that `systemd-run --user --scope` works on this host.
pub async fn probe_systemd_scope() -> bool {
    let mut cmd = Command::new("systemd-run");
    inherit_env(&mut cmd);
    let res = tokio::time::timeout(
        Duration::from_secs(10),
        cmd.args(["--user", "--scope", "--quiet", "--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
    matches!(res, Ok(Ok(st)) if st.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_proto::{ClientIdentity, Priority, ToolchainFingerprint};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn req(cwd: PathBuf, argv: &[&str]) -> JobRequest {
        JobRequest {
            cwd,
            argv: argv.iter().map(|s| s.to_string()).collect(),
            env: BTreeMap::from([("MY".to_string(), "1".to_string())]),
            toolchain: ToolchainFingerprint {
                rustc_commit: "x".into(),
                rustc_version: "1.0".into(),
                host: "h".into(),
                cargo_version: "cargo".into(),
                toolchain_name: String::new(),
            },
            priority: Priority::Background,
            client: ClientIdentity {
                hostname: "c".into(),
                pid: 1,
                label: None,
            },
            tty: false,
        }
    }

    fn runner(timeout: Option<Duration>) -> Runner {
        Runner::new(
            "--jobserver-auth=fifo:/tmp/x".into(),
            Scope::None,
            timeout,
            "srv".into(),
        )
    }

    #[test]
    fn validate_cwd_rules() {
        assert!(Runner::validate_cwd(Path::new("relative")).is_err());
        assert!(Runner::validate_cwd(Path::new("/definitely/not/here/rbs")).is_err());
        assert!(Runner::validate_cwd(Path::new("/")).is_ok());
    }

    #[test]
    fn scoped_command_uses_systemd_run_with_limits() {
        let r = Runner::new(
            "mf".into(),
            Scope::Systemd { mem_max_gib: 7 },
            None,
            "srv".into(),
        );
        let cmd = r.build_command(&req(PathBuf::from("/"), &["cargo", "build"]), r.scope);
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "systemd-run");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            vec![
                "--user",
                "--scope",
                "--quiet",
                "-p",
                "MemoryMax=7G",
                "-p",
                "CPUWeight=50",
                "--",
                "cargo",
                "build"
            ]
        );
        let env: BTreeMap<String, String> = std_cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(env.get("MAKEFLAGS").map(String::as_str), Some("mf"));
        assert_eq!(env.get("CARGO_MAKEFLAGS").map(String::as_str), Some("mf"));
        assert_eq!(env.get("RUSTC_WRAPPER").map(String::as_str), Some("kache"));
        assert_eq!(env.get("RBS_SHIM_ACTIVE").map(String::as_str), Some("1"));
        assert_eq!(env.get("MY").map(String::as_str), Some("1"));
        assert_eq!(std_cmd.get_current_dir(), Some(Path::new("/")));
    }

    #[tokio::test]
    async fn runs_and_reports_exit_code() {
        let r = runner(None);
        let (tx, mut rx) = mpsc::channel(16);
        let h = r
            .spawn(
                JobId(1),
                &req(PathBuf::from("/"), &["sh", "-c", "echo hi; exit 4"]),
                tx,
            )
            .expect("spawn");
        let (_cancel, task) = h.into_parts();
        task.await.expect("join");
        let mut evs = Vec::new();
        while let Some(e) = rx.recv().await {
            evs.push(e);
        }
        assert!(matches!(evs[0], JobEvent::Started { id: JobId(1) }));
        assert!(
            evs.iter()
                .any(|e| matches!(e, JobEvent::Stdout { bytes, .. } if bytes == b"hi\n"))
        );
        assert_eq!(
            evs.last(),
            Some(&JobEvent::Exited {
                id: JobId(1),
                status: ExitStatus::Code(4)
            })
        );
    }

    #[tokio::test]
    async fn dropping_handle_terminates_job() {
        let r = runner(None);
        let (tx, mut rx) = mpsc::channel(16);
        let h = r
            .spawn(JobId(2), &req(PathBuf::from("/"), &["sleep", "30"]), tx)
            .expect("spawn");
        assert!(matches!(rx.recv().await, Some(JobEvent::Started { .. })));
        let (cancel, task) = h.into_parts();
        drop(cancel);
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("terminates promptly")
            .expect("join");
        let mut last = None;
        while let Some(e) = rx.recv().await {
            last = Some(e);
        }
        assert_eq!(
            last,
            Some(JobEvent::Exited {
                id: JobId(2),
                status: ExitStatus::Signal(15)
            })
        );
    }

    #[tokio::test]
    async fn timeout_terminates_job() {
        let r = runner(Some(Duration::from_millis(200)));
        let (tx, mut rx) = mpsc::channel(16);
        let h = r
            .spawn(JobId(3), &req(PathBuf::from("/"), &["sleep", "30"]), tx)
            .expect("spawn");
        let (_cancel, task) = h.into_parts();
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .expect("terminates promptly")
            .expect("join");
        let mut last = None;
        while let Some(e) = rx.recv().await {
            last = Some(e);
        }
        assert!(matches!(
            last,
            Some(JobEvent::Exited {
                status: ExitStatus::Signal(15),
                ..
            })
        ));
    }

    #[tokio::test]
    async fn spawn_failure_is_internal_reject() {
        let r = runner(None);
        let (tx, _rx) = mpsc::channel(16);
        let err = r
            .spawn(JobId(4), &req(PathBuf::from("/"), &["/no/such/binary"]), tx)
            .expect_err("must fail");
        assert!(matches!(*err, RejectReason::Internal(_)));
        let (tx, _rx) = mpsc::channel(16);
        assert!(matches!(
            r.spawn(JobId(5), &req(PathBuf::from("/"), &[]), tx)
                .map_err(|e| *e),
            Err(RejectReason::Internal(_))
        ));
    }
}
