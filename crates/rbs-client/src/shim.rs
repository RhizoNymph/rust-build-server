//! The cargo shim flow (docs/features/client.md). The core `run` is generic
//! over [`Hooks`] so every side effect (transports, sync, exec, output,
//! signals) is injectable and the flow can be tested against `FakeTransport`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rbs_config::{Config, Mode, PostBuild};
use rbs_proto::{
    ClientIdentity, ClientMessage, ExitStatus, JobEvent, JobId, JobRequest, RejectReason,
    ServerMessage, ToolchainFingerprint,
};
use rbs_sync::SyncError;
use rbs_toolchain::ToolchainError;

use crate::backend::{Backend, RemoteProbe, select};
use crate::real_cargo::SHIM_ACTIVE_ENV;
use crate::transport::{Conn, Transport, TransportError, hello, probe_with_timeout};

/// Everything the flow needs to know about this invocation.
#[derive(Debug, Clone)]
pub struct ShimInput {
    /// Absolute working directory (mirrored on the remote).
    pub cwd: PathBuf,
    /// Full argv; `argv[0]` is `"cargo"` for the shim, arbitrary for `rbs exec`.
    pub argv: Vec<String>,
    pub cfg: Config,
    pub tty: bool,
    pub identity: ClientIdentity,
    /// Already filtered through the allowlist.
    pub env: BTreeMap<String, String>,
    /// Apply cargo policy (`local_subcommands`, post-build pull-and-link).
    pub cargo_policy: bool,
    /// Whether the workspace declares a toolchain contract
    /// (`rust-toolchain.toml` / `rust-toolchain`). A mismatch is only a hard
    /// error when a contract exists; unpinned workspaces fall back with a
    /// loud warning instead.
    pub pinned: bool,
}

impl ShimInput {
    /// Build from the process environment for `argv` (already including `argv[0]`).
    pub fn from_env(cwd: PathBuf, argv: Vec<String>, cfg: Config, cargo_policy: bool) -> Self {
        let pinned = rbs_toolchain::find_toolchain_file(&cwd).is_some();
        use std::io::IsTerminal;
        let vars: Vec<(String, String)> = std::env::vars().collect();
        let env = rbs_config::filter_env(
            vars.iter().map(|(k, v)| (k.as_str(), v.as_str())),
            &cfg.policy.env_allowlist,
        );
        let hostname = gethostname::gethostname().to_string_lossy().into_owned();
        Self {
            cwd,
            argv,
            cfg,
            tty: std::io::stdout().is_terminal(),
            identity: ClientIdentity {
                hostname,
                pid: std::process::id(),
                label: std::env::var("RBS_LABEL").ok().filter(|l| !l.is_empty()),
            },
            env,
            cargo_policy,
            pinned,
        }
    }

    fn subcommand(&self) -> Option<&str> {
        self.argv.get(1).map(String::as_str)
    }
}

/// Cargo subcommands that compile (and therefore use the workspace's kache
/// store objects): after one succeeds, the shim fires a detached
/// `rbs store-touch` so the shared store sees this workspace as active.
pub const COMPILING_SUBCOMMANDS: &[&str] =
    &["bench", "build", "check", "clippy", "doc", "run", "test"];

/// Whether this invocation should fire a store touch: cargo policy applies,
/// `[store] touch` is enabled, and the subcommand compiles.
fn wants_touch(input: &ShimInput, sub: Option<&str>) -> bool {
    input.cargo_policy
        && input.cfg.store.touch
        && sub.is_some_and(|s| COMPILING_SUBCOMMANDS.contains(&s))
}

/// Injectable side effects.
pub trait Hooks: Send + Sync {
    /// `None` = no remote configured for this run.
    fn remote(&self) -> Option<Box<dyn Transport>>;
    /// `None` = no local server configured for this run.
    fn local(&self) -> Option<Box<dyn Transport>>;
    fn fingerprint(&self, cwd: &Path) -> Result<ToolchainFingerprint, ToolchainError>;
    fn workspace_root(&self, cwd: &Path)
    -> impl Future<Output = Result<PathBuf, SyncError>> + Send;
    fn push(&self, root: &Path, host: &str) -> impl Future<Output = Result<(), SyncError>> + Send;
    fn pull(&self, root: &Path) -> impl Future<Output = Result<(), SyncError>> + Send;
    /// Run `argv` on this machine (exec in production). Returns the exit code
    /// if it returns at all.
    fn exec_local(&self, argv: &[String]) -> i32;
    /// Fire-and-forget `rbs store-touch --workspace <dir>`, fully detached.
    /// Must never block or affect the build's exit code; `dir` may be any
    /// directory inside the workspace (the touch resolves the root itself).
    fn spawn_touch(&self, dir: &Path);
    fn stdout(&self, bytes: &[u8]);
    fn stderr(&self, bytes: &[u8]);
    /// Resolves when the user asked to interrupt (SIGINT/SIGTERM).
    fn cancel_signal(&self) -> impl Future<Output = ()> + Send;
}

/// How one submission ended.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Outcome {
    Exited(ExitStatus),
    ToolchainMismatch {
        server_host: String,
        server: ToolchainFingerprint,
        client: ToolchainFingerprint,
    },
    Saturated,
    Rejected(String),
    Transport(TransportError),
}

/// Run the shim flow; returns the process exit code.
/// Cargo subcommands that never compile anything (or must act on the local
/// checkout) and therefore always go straight to the real cargo. This is also
/// a structural guard: tools that call `cargo -V` / `cargo metadata` through
/// PATH (including rbs itself) can never turn into remote jobs.
pub const PASSTHROUGH_SUBCOMMANDS: &[&str] = &[
    "metadata",
    "locate-project",
    "pkgid",
    "tree",
    "search",
    "login",
    "logout",
    "install",
    "uninstall",
    "new",
    "init",
    "add",
    "remove",
    "rm",
    "update",
    "fetch",
    "generate-lockfile",
    "vendor",
    "publish",
    "package",
    "owner",
    "yank",
    "version",
    "help",
    "fmt",
    "read-manifest",
    "verify-project",
    "report",
    "config",
    "info",
    "clean",
];

/// `None` (bare `cargo`), a leading flag (`-V`, `--list`, `--version`), or a
/// subcommand in [`PASSTHROUGH_SUBCOMMANDS`] → run the real cargo locally.
pub fn is_passthrough(sub: Option<&str>) -> bool {
    match sub {
        None => true,
        Some(s) => s.starts_with('-') || PASSTHROUGH_SUBCOMMANDS.contains(&s),
    }
}

pub async fn run<H: Hooks>(input: ShimInput, hooks: &H) -> i32 {
    let mode = input.cfg.policy.mode;
    let sub = input.subcommand().map(str::to_string);

    // Exec replaces the process, so for locally exec'd builds the touch must
    // be spawned *before* the exec (the detached child outlives it).
    let touch_before_exec = |dir: &Path| {
        if wants_touch(&input, sub.as_deref()) {
            hooks.spawn_touch(dir);
        }
    };

    if mode == Mode::Plain {
        tracing::debug!("mode=plain; running locally");
        touch_before_exec(&input.cwd);
        return hooks.exec_local(&input.argv);
    }
    if input.cargo_policy && is_passthrough(sub.as_deref()) {
        tracing::debug!(argv = ?input.argv, "non-build cargo invocation; running locally");
        return hooks.exec_local(&input.argv);
    }
    if input.cargo_policy
        && let Some(s) = &sub
        && input.cfg.policy.local_subcommands.iter().any(|l| l == s)
    {
        tracing::debug!(subcommand = %s, "local_subcommands bypass; running locally");
        touch_before_exec(&input.cwd);
        return hooks.exec_local(&input.argv);
    }

    let toolchain = match hooks.fingerprint(&input.cwd) {
        Ok(fp) => fp,
        Err(e) => {
            if mode == Mode::Auto {
                tracing::warn!(error = %e, "could not fingerprint toolchain; running locally");
                touch_before_exec(&input.cwd);
                return hooks.exec_local(&input.argv);
            }
            tracing::error!(error = %e, "could not fingerprint toolchain");
            return 1;
        }
    };

    // Probe the remote once; keep the handshaken connection for submission.
    let mut remote_conn: Option<(Box<dyn Conn>, String)> = None;
    let remote_probe = if matches!(mode, Mode::Auto | Mode::Remote) && input.cfg.remote.enabled {
        match hooks.remote() {
            Some(t) => {
                let timeout = Duration::from_millis(input.cfg.remote.connect_timeout_ms);
                match probe_remote(t.as_ref(), timeout).await {
                    Ok((conn, pr)) => {
                        tracing::debug!(host = %t.describe(), rtt_ms = pr.rtt.as_millis() as u64, "remote probe ok");
                        remote_conn = Some((conn, pr.status.identity.hostname.clone()));
                        Some(RemoteProbe::Ok(pr))
                    }
                    Err(e) => Some(RemoteProbe::Failed(e.to_string())),
                }
            }
            None => Some(RemoteProbe::Failed("no remote transport".into())),
        }
    } else {
        None
    };

    let local_available = hooks.local().is_some();
    let decision = select(mode, remote_probe.as_ref(), local_available, &input.cfg);
    for s in &decision.skipped {
        tracing::warn!(backend = %s.backend, reason = %s.reason, "skipping backend");
    }
    if decision.chain.is_empty() {
        tracing::error!(mode = ?mode, "no backend available");
        return 1;
    }

    let request = JobRequest {
        cwd: input.cwd.clone(),
        argv: input.argv.clone(),
        env: job_env(&input.env),
        toolchain,
        priority: input.cfg.policy.priority,
        client: input.identity.clone(),
        tty: input.tty,
    };

    for backend in decision.chain {
        let outcome = match backend {
            Backend::Plain => {
                tracing::debug!("running plain cargo locally");
                touch_before_exec(&input.cwd);
                return hooks.exec_local(&input.argv);
            }
            Backend::Remote => {
                let Some((conn, server_host)) = remote_conn.take() else {
                    tracing::warn!("remote selected without a connection; skipping");
                    continue;
                };
                let root = match hooks.workspace_root(&input.cwd).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(error = %e, "cannot determine workspace root; skipping remote");
                        continue;
                    }
                };
                if let Err(e) = hooks.push(&root, &input.cfg.remote.host).await {
                    tracing::warn!(error = %e, host = %input.cfg.remote.host, "rsync push failed; skipping remote");
                    continue;
                }
                let mut conn = conn;
                let outcome = submit(conn.as_mut(), &request, &server_host, hooks).await;
                if let Outcome::Exited(status) = &outcome
                    && status.success()
                    && input.cargo_policy
                    && sub.as_deref() == Some("build")
                    && input.cfg.policy.post_build == PostBuild::PullAndLink
                {
                    // This path exec's the local link step, so the touch must
                    // be spawned first (root is already resolved here).
                    if wants_touch(&input, sub.as_deref()) {
                        hooks.spawn_touch(&root);
                    }
                    if let Err(e) = hooks.pull(&root).await {
                        tracing::warn!(error = %e, "kache pull failed; local build will rebuild misses");
                    }
                    tracing::debug!("post-build: linking locally");
                    return hooks.exec_local(&input.argv);
                }
                outcome
            }
            Backend::Local => {
                let Some(t) = hooks.local() else {
                    continue;
                };
                let mut conn = match t.connect().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, socket = %t.describe(), "local server unavailable; skipping");
                        continue;
                    }
                };
                let server_host = match hello(conn.as_mut()).await {
                    Ok(id) => id.hostname,
                    Err(e) => {
                        tracing::warn!(error = %e, "local server handshake failed; skipping");
                        continue;
                    }
                };
                submit(conn.as_mut(), &request, &server_host, hooks).await
            }
        };

        match outcome {
            Outcome::Exited(status) => {
                tracing::debug!(backend = %backend, ?status, "job finished");
                if status.success() && wants_touch(&input, sub.as_deref()) {
                    hooks.spawn_touch(&input.cwd);
                }
                return status.as_process_exit_code();
            }
            Outcome::ToolchainMismatch {
                server_host,
                server,
                client,
            } => {
                let m = rbs_toolchain::Mismatch {
                    client_host: input.identity.hostname.clone(),
                    client,
                    server_host,
                    server,
                };
                if input.pinned {
                    hooks.stderr(format!("rbs: error: {m}").as_bytes());
                    return 1;
                }
                // No rust-toolchain.toml in the workspace: there was never a
                // contract to violate, so degrade instead of breaking the build.
                hooks.stderr(
                    format!(
                        "rbs: warning: {m}  note: this workspace has no rust-toolchain.toml; falling back to a local build. Pin the workspace to build remotely.\n"
                    )
                    .as_bytes(),
                );
                tracing::warn!(backend = %backend, "toolchain mismatch in unpinned workspace; trying next backend");
            }
            Outcome::Saturated => {
                tracing::warn!(backend = %backend, "server saturated; trying next backend");
            }
            Outcome::Rejected(reason) => {
                tracing::warn!(backend = %backend, reason = %reason, "job rejected; trying next backend");
            }
            Outcome::Transport(e) => {
                tracing::warn!(backend = %backend, error = %e, "connection failed; trying next backend");
            }
        }
    }
    tracing::error!("every backend failed");
    1
}

fn job_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut e = env.clone();
    e.insert(SHIM_ACTIVE_ENV.to_string(), "1".to_string());
    e
}

async fn probe_remote(
    t: &dyn Transport,
    timeout: Duration,
) -> Result<(Box<dyn Conn>, crate::transport::ProbeResult), TransportError> {
    let mut conn = match tokio::time::timeout(timeout, t.connect()).await {
        Ok(r) => r?,
        Err(_) => return Err(TransportError::Timeout(timeout)),
    };
    let pr = probe_with_timeout(conn.as_mut(), timeout).await?;
    Ok((conn, pr))
}

/// Submit and stream until a terminal event. Forwards a cancel signal as
/// `Cancel { id }` and keeps draining so the final status is observed.
async fn submit<H: Hooks>(
    conn: &mut dyn Conn,
    request: &JobRequest,
    server_host: &str,
    hooks: &H,
) -> Outcome {
    if let Err(e) = conn.send(ClientMessage::Submit(request.clone())).await {
        return Outcome::Transport(e);
    }
    let mut id: Option<JobId> = None;
    let mut cancelled = false;
    let cancel = hooks.cancel_signal();
    tokio::pin!(cancel);
    loop {
        let msg = tokio::select! {
            m = conn.recv() => m,
            () = &mut cancel, if !cancelled => {
                cancelled = true;
                if let Some(id) = id {
                    tracing::warn!(?id, "interrupted; cancelling job");
                    if let Err(e) = conn.send(ClientMessage::Cancel { id }).await {
                        return Outcome::Transport(e);
                    }
                } else {
                    tracing::warn!("interrupted before the job was assigned; giving up");
                    return Outcome::Exited(ExitStatus::Signal(2));
                }
                continue;
            }
        };
        let ev = match msg {
            Ok(Some(ServerMessage::Event(ev))) => ev,
            Ok(Some(ServerMessage::Error(e))) => {
                return Outcome::Transport(TransportError::Protocol(e.message));
            }
            Ok(Some(other)) => {
                return Outcome::Transport(TransportError::Unexpected(format!("{other:?}")));
            }
            Ok(None) => return Outcome::Transport(TransportError::Closed),
            Err(e) => return Outcome::Transport(e),
        };
        if id.is_none() {
            id = Some(ev.id());
        }
        match ev {
            JobEvent::Queued { position, .. } => {
                tracing::info!(position, "queued (position {position})");
            }
            JobEvent::Started { .. } => {
                tracing::debug!(server = server_host, "job started");
            }
            JobEvent::Stdout { bytes, .. } => hooks.stdout(&bytes),
            JobEvent::Stderr { bytes, .. } => hooks.stderr(&bytes),
            JobEvent::Exited { status, .. } => return Outcome::Exited(status),
            JobEvent::Rejected { reason, .. } => {
                return match reason {
                    RejectReason::ToolchainMismatch { server, client } => {
                        Outcome::ToolchainMismatch {
                            server_host: server_host.to_string(),
                            server,
                            client,
                        }
                    }
                    RejectReason::Saturated => Outcome::Saturated,
                    RejectReason::VersionMismatch => {
                        Outcome::Rejected("protocol version mismatch".into())
                    }
                    RejectReason::Internal(m) => Outcome::Rejected(format!("internal: {m}")),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::FakeTransport;
    use rbs_proto::{Capacity, PROTOCOL_VERSION, ServerIdentity, ServerStatus};
    use std::sync::Mutex;

    fn fp(commit: &str) -> ToolchainFingerprint {
        ToolchainFingerprint {
            rustc_commit: commit.into(),
            rustc_version: "1.95.0".into(),
            host: "x86_64-unknown-linux-gnu".into(),
            cargo_version: "cargo 1.95.0".into(),
            toolchain_name: String::new(),
        }
    }

    fn hello_msg(host: &str) -> ServerMessage {
        ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            server: ServerIdentity {
                hostname: host.into(),
                version: "0.1.0".into(),
            },
        }
    }

    fn status_msg(accepting: bool) -> ServerMessage {
        ServerMessage::Status(ServerStatus {
            identity: ServerIdentity {
                hostname: "node0".into(),
                version: "0.1.0".into(),
            },
            capacity: Capacity {
                tokens_total: 30,
                tokens_free: 10,
                mem_available_bytes: 1 << 36,
                load1: 1.0,
                accepting,
            },
            queued: 0,
            running: 0,
            uptime_secs: 5,
        })
    }

    fn ev(e: JobEvent) -> ServerMessage {
        ServerMessage::Event(e)
    }

    #[derive(Default)]
    struct Calls {
        exec: Vec<Vec<String>>,
        pushes: Vec<(PathBuf, String)>,
        pulls: Vec<PathBuf>,
        touches: Vec<PathBuf>,
        /// Interleaved order of side effects ("touch", "exec").
        events: Vec<&'static str>,
        out: Vec<u8>,
        err: Vec<u8>,
    }

    struct TestHooks {
        remote: Option<FakeTransport>,
        local: Option<FakeTransport>,
        fingerprint: Result<ToolchainFingerprint, String>,
        push_result: Result<(), String>,
        exec_code: i32,
        calls: Mutex<Calls>,
    }

    impl TestHooks {
        fn new(remote: Option<FakeTransport>, local: Option<FakeTransport>) -> Self {
            Self {
                remote,
                local,
                fingerprint: Ok(fp("abc")),
                push_result: Ok(()),
                exec_code: 0,
                calls: Mutex::new(Calls::default()),
            }
        }
        fn calls(&self) -> std::sync::MutexGuard<'_, Calls> {
            self.calls.lock().unwrap_or_else(|e| e.into_inner())
        }
    }

    impl Hooks for TestHooks {
        fn remote(&self) -> Option<Box<dyn Transport>> {
            self.remote
                .clone()
                .map(|t| Box::new(t) as Box<dyn Transport>)
        }
        fn local(&self) -> Option<Box<dyn Transport>> {
            self.local
                .clone()
                .map(|t| Box::new(t) as Box<dyn Transport>)
        }
        fn fingerprint(&self, _cwd: &Path) -> Result<ToolchainFingerprint, ToolchainError> {
            self.fingerprint
                .clone()
                .map_err(|field| ToolchainError::Parse {
                    field: Box::leak(field.into_boxed_str()),
                })
        }
        async fn workspace_root(&self, cwd: &Path) -> Result<PathBuf, SyncError> {
            Ok(cwd.to_path_buf())
        }
        async fn push(&self, root: &Path, host: &str) -> Result<(), SyncError> {
            self.calls()
                .pushes
                .push((root.to_path_buf(), host.to_string()));
            self.push_result.clone().map_err(|m| SyncError::Failed {
                program: "rsync".into(),
                status: "exit status: 1".into(),
                stderr: m,
            })
        }
        async fn pull(&self, root: &Path) -> Result<(), SyncError> {
            self.calls().pulls.push(root.to_path_buf());
            Ok(())
        }
        fn exec_local(&self, argv: &[String]) -> i32 {
            let mut calls = self.calls();
            calls.exec.push(argv.to_vec());
            calls.events.push("exec");
            self.exec_code
        }
        fn spawn_touch(&self, dir: &Path) {
            let mut calls = self.calls();
            calls.touches.push(dir.to_path_buf());
            calls.events.push("touch");
        }
        fn stdout(&self, bytes: &[u8]) {
            self.calls().out.extend_from_slice(bytes);
        }
        fn stderr(&self, bytes: &[u8]) {
            self.calls().err.extend_from_slice(bytes);
        }
        async fn cancel_signal(&self) {
            std::future::pending::<()>().await
        }
    }

    fn input(args: &[&str], mode: Mode) -> ShimInput {
        let mut cfg = Config::default();
        cfg.policy.mode = mode;
        cfg.remote.max_rtt_ms = 10_000;
        ShimInput {
            cwd: PathBuf::from("/w"),
            argv: std::iter::once("cargo")
                .chain(args.iter().copied())
                .map(String::from)
                .collect(),
            cfg,
            tty: false,
            identity: ClientIdentity {
                hostname: "laptop".into(),
                pid: 7,
                label: Some("agent-1".into()),
            },
            env: BTreeMap::from([("RUSTFLAGS".to_string(), "-Cdebuginfo=0".to_string())]),
            cargo_policy: true,
            pinned: true,
        }
    }

    fn job_events(code: i32) -> Vec<ServerMessage> {
        vec![
            ev(JobEvent::Queued {
                id: JobId(1),
                position: 2,
            }),
            ev(JobEvent::Started { id: JobId(1) }),
            ev(JobEvent::Stdout {
                id: JobId(1),
                bytes: b"out1\n".to_vec(),
            }),
            ev(JobEvent::Stderr {
                id: JobId(1),
                bytes: b"err1\n".to_vec(),
            }),
            ev(JobEvent::Exited {
                id: JobId(1),
                status: ExitStatus::Code(code),
            }),
        ]
    }

    #[tokio::test]
    async fn remote_job_streams_to_fds_and_propagates_exit_code() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(3));
        let remote = FakeTransport::scripted(script);
        let hooks = TestHooks::new(Some(remote.clone()), Some(FakeTransport::default()));
        let code = run(input(&["test", "--lib"], Mode::Auto), &hooks).await;
        assert_eq!(code, 3);
        let calls = hooks.calls();
        assert_eq!(calls.out, b"out1\n");
        assert_eq!(calls.err, b"err1\n");
        assert!(calls.exec.is_empty(), "test is not post-build linked");
        assert_eq!(
            calls.pushes,
            vec![(PathBuf::from("/w"), "node0".to_string())]
        );
        let sent = remote.sent();
        assert_eq!(
            sent[0],
            ClientMessage::Hello {
                version: PROTOCOL_VERSION
            }
        );
        assert_eq!(sent[1], ClientMessage::Status);
        match &sent[2] {
            ClientMessage::Submit(req) => {
                assert_eq!(req.argv, vec!["cargo", "test", "--lib"]);
                assert_eq!(req.cwd, PathBuf::from("/w"));
                assert_eq!(
                    req.env.get("RUSTFLAGS").map(String::as_str),
                    Some("-Cdebuginfo=0")
                );
                assert_eq!(req.env.get(SHIM_ACTIVE_ENV).map(String::as_str), Some("1"));
                assert_eq!(req.client.label.as_deref(), Some("agent-1"));
                assert_eq!(req.toolchain, fp("abc"));
            }
            other => panic!("expected Submit, got {other:?}"),
        }
        assert_eq!(sent.len(), 3);
    }

    #[tokio::test]
    async fn signal_exit_maps_to_128_plus() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.push(ev(JobEvent::Started { id: JobId(1) }));
        script.push(ev(JobEvent::Exited {
            id: JobId(1),
            status: ExitStatus::Signal(9),
        }));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        assert_eq!(run(input(&["check"], Mode::Auto), &hooks).await, 137);
    }

    #[tokio::test]
    async fn unpinned_mismatch_warns_and_falls_back() {
        let script = vec![
            hello_msg("node0"),
            status_msg(true),
            ev(JobEvent::Rejected {
                id: JobId(1),
                reason: RejectReason::ToolchainMismatch {
                    server: fp("def"),
                    client: fp("abc"),
                },
            }),
        ];
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let hooks = TestHooks::new(
            Some(FakeTransport::scripted(script)),
            Some(FakeTransport::scripted(local_script)),
        );
        let mut inp = input(&["build"], Mode::Auto);
        inp.pinned = false;
        let code = run(inp, &hooks).await;
        assert_eq!(code, 0, "must fall through to the local backend");
        let err = String::from_utf8_lossy(&hooks.calls().err).to_string();
        assert!(
            err.contains("no rust-toolchain.toml"),
            "warning must explain the fallback: {err}"
        );
    }

    #[tokio::test]
    async fn toolchain_mismatch_exits_1_without_fallback() {
        let script = vec![
            hello_msg("node0"),
            status_msg(true),
            ev(JobEvent::Rejected {
                id: JobId(1),
                reason: RejectReason::ToolchainMismatch {
                    server: fp("def"),
                    client: fp("abc"),
                },
            }),
        ];
        let local = FakeTransport::scripted(vec![hello_msg("laptop")]);
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), Some(local.clone()));
        let code = run(input(&["build"], Mode::Auto), &hooks).await;
        assert_eq!(code, 1);
        let calls = hooks.calls();
        let err = String::from_utf8_lossy(&calls.err);
        assert!(
            err.contains("toolchain mismatch between laptop and node0"),
            "{err}"
        );
        assert!(err.contains("laptop:  rustc 1.95.0 (abc)"));
        assert!(err.contains("node0:   rustc 1.95.0 (def)"));
        assert!(calls.exec.is_empty(), "no plain fallback");
        assert!(calls.pulls.is_empty());
        assert_eq!(local.connect_count(), 0, "no local fallback");
    }

    #[tokio::test]
    async fn saturated_remote_falls_to_local() {
        let remote = FakeTransport::scripted(vec![
            hello_msg("node0"),
            status_msg(true),
            ev(JobEvent::Rejected {
                id: JobId(1),
                reason: RejectReason::Saturated,
            }),
        ]);
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let local = FakeTransport::scripted(local_script);
        let hooks = TestHooks::new(Some(remote.clone()), Some(local.clone()));
        let code = run(input(&["build"], Mode::Auto), &hooks).await;
        assert_eq!(code, 0);
        assert_eq!(local.connect_count(), 1);
        let sent = local.sent();
        assert!(matches!(sent[0], ClientMessage::Hello { .. }));
        assert!(matches!(sent[1], ClientMessage::Submit(_)));
        let calls = hooks.calls();
        assert_eq!(calls.out, b"out1\n");
        assert!(
            calls.exec.is_empty(),
            "pull-and-link only after a remote build"
        );
        assert!(calls.pulls.is_empty());
    }

    #[test]
    fn passthrough_table() {
        assert!(is_passthrough(None));
        assert!(is_passthrough(Some("-V")));
        assert!(is_passthrough(Some("--version")));
        assert!(is_passthrough(Some("metadata")));
        assert!(is_passthrough(Some("locate-project")));
        assert!(!is_passthrough(Some("build")));
        assert!(!is_passthrough(Some("test")));
        assert!(!is_passthrough(Some("check")));
        assert!(!is_passthrough(Some("clippy")));
    }

    #[tokio::test]
    async fn passthrough_invocations_never_touch_backends() {
        for args in [
            vec!["-V"],
            vec!["metadata", "--format-version", "1"],
            vec![],
        ] {
            let remote = FakeTransport::scripted(vec![hello_msg("node0"), status_msg(true)]);
            let hooks = TestHooks::new(Some(remote.clone()), Some(FakeTransport::default()));
            let hooks = TestHooks {
                exec_code: 7,
                ..hooks
            };
            let code = run(input(&args, Mode::Auto), &hooks).await;
            assert_eq!(code, 7, "{args:?}");
            assert_eq!(remote.connect_count(), 0, "{args:?}");
            let calls = hooks.calls();
            assert_eq!(calls.exec.len(), 1, "{args:?}");
            assert_eq!(calls.exec[0][0], "cargo", "{args:?}");
        }
    }

    #[tokio::test]
    async fn local_subcommands_bypass_everything() {
        let remote = FakeTransport::scripted(vec![hello_msg("node0"), status_msg(true)]);
        let hooks = TestHooks::new(Some(remote.clone()), Some(FakeTransport::default()));
        let hooks = TestHooks {
            exec_code: 5,
            ..hooks
        };
        let code = run(input(&["run", "--", "x"], Mode::Auto), &hooks).await;
        assert_eq!(code, 5);
        assert_eq!(remote.connect_count(), 0);
        assert_eq!(hooks.calls().exec, vec![vec!["cargo", "run", "--", "x"]]);
    }

    #[tokio::test]
    async fn plain_mode_execs_directly() {
        let remote = FakeTransport::scripted(vec![hello_msg("node0"), status_msg(true)]);
        let hooks = TestHooks::new(Some(remote.clone()), None);
        assert_eq!(run(input(&["build"], Mode::Plain), &hooks).await, 0);
        assert_eq!(remote.connect_count(), 0);
        assert_eq!(hooks.calls().exec.len(), 1);
    }

    #[tokio::test]
    async fn remote_build_pulls_and_links_locally() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(0));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        let hooks = TestHooks {
            exec_code: 0,
            ..hooks
        };
        let code = run(input(&["build", "--release"], Mode::Auto), &hooks).await;
        assert_eq!(code, 0);
        let calls = hooks.calls();
        assert_eq!(calls.pulls, vec![PathBuf::from("/w")]);
        assert_eq!(calls.exec, vec![vec!["cargo", "build", "--release"]]);
    }

    #[tokio::test]
    async fn failed_remote_build_does_not_pull() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(101));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 101);
        let calls = hooks.calls();
        assert!(calls.pulls.is_empty());
        assert!(calls.exec.is_empty());
    }

    #[tokio::test]
    async fn push_failure_falls_through_to_local() {
        let remote = FakeTransport::scripted(vec![hello_msg("node0"), status_msg(true)]);
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let local = FakeTransport::scripted(local_script);
        let hooks = TestHooks {
            push_result: Err("host unreachable".into()),
            ..TestHooks::new(Some(remote.clone()), Some(local.clone()))
        };
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 0);
        assert_eq!(
            remote.sent().len(),
            2,
            "no Submit to remote after push failure"
        );
        assert_eq!(local.connect_count(), 1);
    }

    #[tokio::test]
    async fn remote_probe_failure_goes_local_then_plain() {
        let hooks = TestHooks::new(
            Some(FakeTransport::failing("ssh: connection refused")),
            Some(FakeTransport::failing("no socket")),
        );
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 0);
        assert_eq!(hooks.calls().exec, vec![vec!["cargo", "build"]]);
    }

    #[tokio::test]
    async fn forced_remote_errors_instead_of_falling_back() {
        let hooks = TestHooks::new(
            Some(FakeTransport::failing("ssh: connection refused")),
            Some(FakeTransport::default()),
        );
        assert_eq!(run(input(&["build"], Mode::Remote), &hooks).await, 1);
        assert!(hooks.calls().exec.is_empty());
    }

    #[tokio::test]
    async fn not_accepting_remote_is_skipped_without_push() {
        let remote = FakeTransport::scripted(vec![hello_msg("node0"), status_msg(false)]);
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let hooks = TestHooks::new(Some(remote), Some(FakeTransport::scripted(local_script)));
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 0);
        assert!(hooks.calls().pushes.is_empty());
    }

    #[test]
    fn compiling_subcommands_table() {
        for s in ["bench", "build", "check", "clippy", "doc", "run", "test"] {
            assert!(COMPILING_SUBCOMMANDS.contains(&s), "{s}");
        }
        assert!(!COMPILING_SUBCOMMANDS.contains(&"metadata"));
        assert!(!COMPILING_SUBCOMMANDS.contains(&"fmt"));
        assert!(!COMPILING_SUBCOMMANDS.contains(&"clean"));
    }

    #[tokio::test]
    async fn touch_fires_after_successful_remote_build_before_the_link_exec() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(0));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 0);
        let calls = hooks.calls();
        assert_eq!(calls.touches, vec![PathBuf::from("/w")]);
        assert_eq!(
            calls.events,
            vec!["touch", "exec"],
            "touch spawned before the link step exec"
        );
    }

    #[tokio::test]
    async fn touch_fires_after_successful_remote_non_build_compile() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(0));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        assert_eq!(run(input(&["check"], Mode::Auto), &hooks).await, 0);
        let calls = hooks.calls();
        assert_eq!(calls.touches, vec![PathBuf::from("/w")]);
        assert!(calls.exec.is_empty(), "check has no link step");
    }

    #[tokio::test]
    async fn touch_fires_after_successful_local_backend_job() {
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let hooks = TestHooks::new(None, Some(FakeTransport::scripted(local_script)));
        assert_eq!(run(input(&["test"], Mode::Local), &hooks).await, 0);
        assert_eq!(hooks.calls().touches, vec![PathBuf::from("/w")]);
    }

    #[tokio::test]
    async fn touch_fires_before_plain_exec() {
        let hooks = TestHooks::new(None, None);
        assert_eq!(run(input(&["build"], Mode::Plain), &hooks).await, 0);
        let calls = hooks.calls();
        assert_eq!(calls.touches, vec![PathBuf::from("/w")]);
        assert_eq!(
            calls.events,
            vec!["touch", "exec"],
            "exec replaces the process, so the touch must be spawned first"
        );
    }

    #[tokio::test]
    async fn touch_fires_before_plain_backend_fallback_exec() {
        let hooks = TestHooks::new(
            Some(FakeTransport::failing("ssh: connection refused")),
            Some(FakeTransport::failing("no socket")),
        );
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 0);
        let calls = hooks.calls();
        assert_eq!(calls.touches, vec![PathBuf::from("/w")]);
        assert_eq!(calls.events, vec!["touch", "exec"]);
    }

    #[tokio::test]
    async fn touch_fires_before_local_subcommand_exec() {
        let hooks = TestHooks::new(None, None);
        assert_eq!(run(input(&["run", "--", "x"], Mode::Auto), &hooks).await, 0);
        let calls = hooks.calls();
        assert_eq!(calls.touches, vec![PathBuf::from("/w")]);
        assert_eq!(calls.events, vec!["touch", "exec"]);
    }

    #[tokio::test]
    async fn touch_not_fired_on_a_failed_build() {
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(101));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        assert_eq!(run(input(&["build"], Mode::Auto), &hooks).await, 101);
        assert!(hooks.calls().touches.is_empty());

        let hooks = TestHooks {
            exec_code: 1,
            ..TestHooks::new(None, None)
        };
        // plain exec fires pre-exec by design, so a *failing plain* build does
        // touch; but a non-compiling plain invocation must not.
        assert_eq!(run(input(&["metadata"], Mode::Plain), &hooks).await, 1);
        assert!(hooks.calls().touches.is_empty());
    }

    #[tokio::test]
    async fn touch_not_fired_for_passthrough_subcommands() {
        for args in [vec!["metadata"], vec!["fmt"], vec!["-V"], vec![]] {
            let hooks = TestHooks::new(None, None);
            run(input(&args, Mode::Auto), &hooks).await;
            assert!(hooks.calls().touches.is_empty(), "{args:?}");
        }
    }

    #[tokio::test]
    async fn touch_not_fired_when_disabled_in_config() {
        // plain exec path
        let hooks = TestHooks::new(None, None);
        let mut inp = input(&["build"], Mode::Plain);
        inp.cfg.store.touch = false;
        assert_eq!(run(inp, &hooks).await, 0);
        assert!(hooks.calls().touches.is_empty());

        // server-backed path
        let mut script = vec![hello_msg("node0"), status_msg(true)];
        script.extend(job_events(0));
        let hooks = TestHooks::new(Some(FakeTransport::scripted(script)), None);
        let mut inp = input(&["check"], Mode::Auto);
        inp.cfg.store.touch = false;
        assert_eq!(run(inp, &hooks).await, 0);
        assert!(hooks.calls().touches.is_empty());
    }

    #[tokio::test]
    async fn touch_not_fired_for_rbs_exec() {
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let hooks = TestHooks::new(None, Some(FakeTransport::scripted(local_script)));
        let mut inp = input(&["build"], Mode::Local);
        inp.cargo_policy = false; // `rbs exec -- cargo build`
        assert_eq!(run(inp, &hooks).await, 0);
        assert!(hooks.calls().touches.is_empty());
    }

    #[tokio::test]
    async fn exec_without_cargo_policy_ignores_local_subcommands() {
        let mut local_script = vec![hello_msg("laptop")];
        local_script.extend(job_events(0));
        let hooks = TestHooks::new(None, Some(FakeTransport::scripted(local_script)));
        let mut inp = input(&["run"], Mode::Local);
        inp.cargo_policy = false;
        inp.argv = vec!["sh".into(), "-c".into(), "run".into()];
        assert_eq!(run(inp, &hooks).await, 0);
        assert!(hooks.calls().exec.is_empty());
    }
}
