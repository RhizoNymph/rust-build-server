//! Resource-aware job server. See `docs/features/server.md`.
//!
//! Public API consumed by the `rbs` binary (crates/rbs-client). Keep these
//! signatures stable; the client is developed against them in parallel.

pub mod error;
pub mod jobserver;
pub mod proxy;
pub mod runner;
pub mod scheduler;
pub mod socket;
pub mod sysinfo;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rbs_proto::ServerIdentity;
use tokio::task::JoinSet;
use tracing::{info, warn};

pub use error::ServerError;
use jobserver::JobServer;
use runner::{Runner, Scope};
use scheduler::{Limits, Scheduler};
use socket::Shared;

/// Command-line overrides for `rbs server`.
#[derive(Debug, Clone, Default)]
pub struct ServerOpts {
    /// Socket path; `None` = `cfg.local_server.socket`.
    pub socket: Option<PathBuf>,
    /// Jobserver pool size; `None` = `cfg.server.tokens` (0 → nproc - reserve_cores).
    pub tokens: Option<u32>,
}

/// Whether jobs are wrapped in transient systemd scopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScopeMode {
    /// Probe `systemd-run --user --scope` at start; use it if it works.
    #[default]
    Auto,
    /// Never wrap (tests, hosts without a user manager).
    Disabled,
}

/// Extra knobs for [`run_server_until`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub scope: ScopeMode,
}

/// How often queued jobs are re-evaluated against fresh memory readings.
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long shutdown waits for cancelled jobs to exit.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(runner::KILL_GRACE.as_secs() + 5);

/// Run the server until SIGTERM/SIGINT. Blocks for the life of the process.
pub async fn run_server(cfg: rbs_config::Config, opts: ServerOpts) -> anyhow::Result<()> {
    run_server_until(cfg, opts, RunOptions::default(), wait_for_signal()).await
}

/// Run the server until `shutdown` resolves. Used by `run_server` and by tests.
pub async fn run_server_until(
    cfg: rbs_config::Config,
    opts: ServerOpts,
    run: RunOptions,
    shutdown: impl std::future::Future<Output = ()>,
) -> anyhow::Result<()> {
    let socket_path = opts
        .socket
        .clone()
        .unwrap_or_else(|| cfg.local_server.socket.clone());
    let tokens = resolve_tokens(opts.tokens, &cfg.server, available_parallelism());
    let listener = socket::bind(&socket_path)?;
    let state_dir = socket_path
        .parent()
        .ok_or_else(|| ServerError::NoParent(socket_path.clone()))?;
    let jobserver = JobServer::create(state_dir, tokens)?;

    let scope = match run.scope {
        ScopeMode::Disabled => Scope::None,
        ScopeMode::Auto => {
            if runner::probe_systemd_scope().await {
                Scope::Systemd {
                    mem_max_gib: cfg.server.job_mem_max_gib,
                }
            } else {
                warn!("systemd-run --user --scope unavailable; jobs will run unscoped");
                Scope::None
            }
        }
    };
    let timeout =
        (cfg.server.job_timeout_secs > 0).then(|| Duration::from_secs(cfg.server.job_timeout_secs));
    let hostname = hostname();
    let runner = Runner::new(jobserver.makeflags(), scope, timeout, hostname.clone());
    let identity = ServerIdentity {
        hostname,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let limits = Limits::from(&cfg.server);
    let shared = Arc::new(Shared::new(
        identity,
        Scheduler::new(limits),
        runner,
        jobserver,
    ));
    info!(
        socket = %socket_path.display(),
        tokens,
        max_jobs = limits.max_jobs,
        min_mem_available_bytes = limits.min_mem_available_bytes,
        queue_limit = limits.queue_limit,
        scope = ?scope,
        "rbs server listening"
    );

    let result = serve(&shared, listener, shutdown).await;

    shared.jobs.close();
    if tokio::time::timeout(SHUTDOWN_GRACE, shared.jobs.wait())
        .await
        .is_err()
    {
        warn!("some jobs did not exit before shutdown grace elapsed");
    }
    remove_socket(&socket_path);
    // Unlink the FIFO by dropping the last reference to the jobserver.
    drop(shared);
    info!(socket = %socket_path.display(), "rbs server stopped");
    result.map_err(Into::into)
}

async fn serve(
    shared: &Arc<Shared>,
    listener: tokio::net::UnixListener,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<(), ServerError> {
    let mut connections = JoinSet::new();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => {
                info!("shutdown requested");
                break;
            }
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(socket::handle_connection(Arc::clone(shared), stream));
                }
                Err(e) => {
                    warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = poll.tick() => shared.poll().await,
            Some(res) = connections.join_next(), if !connections.is_empty() => {
                if let Err(e) = res {
                    warn!(error = %e, "connection task panicked");
                }
            }
        }
    }
    drop(listener);
    // Aborting connection tasks drops their job handles, which cancels jobs.
    connections.shutdown().await;
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "cannot listen for SIGTERM; relying on SIGINT");
            match tokio::signal::ctrl_c().await {
                Ok(()) => {}
                Err(e) => warn!(error = %e, "ctrl_c handler failed"),
            }
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => info!(signal = "SIGTERM", "signal received"),
        r = tokio::signal::ctrl_c() => match r {
            Ok(()) => info!(signal = "SIGINT", "signal received"),
            Err(e) => warn!(error = %e, "ctrl_c handler failed"),
        },
    }
}

fn remove_socket(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "failed to remove socket"),
    }
}

fn available_parallelism() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(1)
}

/// `opts` beats `cfg.server.tokens`; `0` means `nproc - reserve_cores`, min 1.
fn resolve_tokens(override_: Option<u32>, server: &rbs_config::Server, nproc: u32) -> u32 {
    let configured = override_.unwrap_or(server.tokens);
    if configured > 0 {
        configured
    } else {
        nproc.saturating_sub(server.reserve_cores).max(1)
    }
}

fn hostname() -> String {
    nix::unistd::gethostname()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "localhost".to_string())
}

/// Bridge this process's stdin/stdout to the unix socket at `socket`
/// (used as `ssh node0 rbs proxy`). Returns when either side closes.
pub async fn run_proxy(socket: PathBuf) -> anyhow::Result<()> {
    bridge_to_socket(&socket, tokio::io::stdin(), tokio::io::stdout()).await?;
    Ok(())
}

/// Generic form of [`run_proxy`] over arbitrary streams (tested in-process).
pub async fn bridge_to_socket<I, O>(socket: &Path, input: I, output: O) -> Result<(), ServerError>
where
    I: tokio::io::AsyncRead + Unpin,
    O: tokio::io::AsyncWrite + Unpin,
{
    proxy::bridge(socket, input, output).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_resolution() {
        let mut s = rbs_config::Server {
            reserve_cores: 2,
            tokens: 0,
            ..rbs_config::Server::default()
        };
        assert_eq!(resolve_tokens(None, &s, 16), 14);
        assert_eq!(resolve_tokens(None, &s, 2), 1);
        assert_eq!(resolve_tokens(None, &s, 1), 1);
        assert_eq!(resolve_tokens(Some(5), &s, 16), 5);
        assert_eq!(resolve_tokens(Some(0), &s, 16), 14);
        s.tokens = 9;
        assert_eq!(resolve_tokens(None, &s, 16), 9);
        assert_eq!(resolve_tokens(Some(3), &s, 16), 3);
    }

    #[test]
    fn hostname_is_nonempty() {
        assert!(!hostname().is_empty());
    }
}
