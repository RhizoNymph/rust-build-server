//! `rbs` CLI and the `cargo` shim (same binary; dispatched on `argv[0]`).

mod backend;
mod local;
mod real_cargo;
mod shim;
mod status;
mod transport;

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use rbs_config::{Config, Mode};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "rbs", version, about = "Rust build server client")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Probe every backend and print capacity / RTT.
    Status,
    /// Run an arbitrary command through the backend chain (debugging).
    Exec {
        #[arg(long, value_parser = parse_mode)]
        mode: Option<Mode>,
        #[arg(required = true, last = true)]
        cmd: Vec<String>,
    },
    /// Run the job server on this host.
    Server {
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        tokens: Option<u32>,
    },
    /// Bridge stdio to the local server socket (used as `ssh host rbs proxy`).
    Proxy {
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Install systemd units, kache config and the cargo shim.
    Setup {
        /// `client` or `server` (`laptop` / `node0` are legacy aliases).
        #[arg(long, default_value = "client", value_parser = parse_role)]
        role: rbs_setup::Role,
        #[arg(long)]
        remote_host: Option<String>,
        #[arg(long)]
        force: bool,
    },
    /// Provision or upgrade every host of the fleet over ssh.
    Bootstrap {
        /// ssh host that runs the build server (overrides `[bootstrap] server`).
        #[arg(long)]
        server: Option<String>,
        /// Comma-separated ssh hosts that submit jobs (overrides `[bootstrap] clients`).
        #[arg(long, value_delimiter = ',')]
        clients: Vec<String>,
        /// Print the planned actions per host and execute nothing.
        #[arg(long)]
        dry_run: bool,
        /// Pass `--force` to the remote `rbs setup`.
        #[arg(long)]
        force: bool,
    },
    /// Verify the installation.
    Doctor {
        #[arg(long)]
        remote: bool,
    },
    /// Garbage-collect the shared kache S3 store (size cap + LFU eviction).
    StoreGc {
        /// Print the eviction plan without deleting anything.
        #[arg(long)]
        dry_run: bool,
        /// Override `[store] max_size_gib` for this run.
        #[arg(long)]
        max_size_gib: Option<u32>,
    },
    /// Refresh S3 last-modified on this workspace's store objects so the GC
    /// sees them as in use (throttled per workspace).
    StoreTouch {
        /// Directory inside the workspace to touch (default: cwd).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Run even if the throttle stamp is fresh.
        #[arg(long)]
        force: bool,
    },
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    s.parse()
}

fn parse_role(s: &str) -> Result<rbs_setup::Role, String> {
    s.parse()
}

/// Writer that prefixes every log line with `rbs: ` so agents can spot it.
struct PrefixedStderr;

impl std::io::Write for PrefixedStderr {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut line = Vec::with_capacity(buf.len() + 5);
        line.extend_from_slice(b"rbs: ");
        line.extend_from_slice(buf);
        std::io::stderr().write_all(&line)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stderr().flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for PrefixedStderr {
    type Writer = PrefixedStderr;
    fn make_writer(&'a self) -> Self::Writer {
        PrefixedStderr
    }
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("RBS_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(PrefixedStderr)
        .with_target(false)
        .without_time()
        .with_ansi(false)
        .init();
}

/// Production [`shim::Hooks`].
struct RealHooks {
    cfg: Config,
    exe: PathBuf,
}

impl shim::Hooks for RealHooks {
    fn remote(&self) -> Option<Box<dyn transport::Transport>> {
        Some(Box::new(transport::SshTransport {
            host: self.cfg.remote.host.clone(),
            connect_timeout_ms: self.cfg.remote.connect_timeout_ms,
            remote_bin: self.cfg.remote.remote_bin.clone(),
        }))
    }
    fn local(&self) -> Option<Box<dyn transport::Transport>> {
        self.cfg.local_server.enabled.then(|| {
            Box::new(local::LocalTransport {
                socket: self.cfg.local_server.socket.clone(),
                autostart: self.cfg.local_server.autostart,
                exe: self.exe.clone(),
            }) as Box<dyn transport::Transport>
        })
    }
    fn fingerprint(
        &self,
        cwd: &Path,
    ) -> Result<rbs_toolchain::ToolchainFingerprint, rbs_toolchain::ToolchainError> {
        rbs_toolchain::fingerprint(cwd)
    }
    async fn workspace_root(&self, cwd: &Path) -> Result<PathBuf, rbs_sync::SyncError> {
        rbs_sync::workspace_root(cwd).await
    }
    async fn push(&self, root: &Path, host: &str) -> Result<(), rbs_sync::SyncError> {
        rbs_sync::push(&self.cfg.sync, root, host).await
    }
    async fn pull(&self, root: &Path) -> Result<(), rbs_sync::SyncError> {
        rbs_sync::pull(root).await
    }
    fn exec_local(&self, argv: &[String]) -> i32 {
        let err = if argv.first().map(String::as_str) == Some("cargo") {
            real_cargo::exec_real_cargo(&argv[1..]).to_string()
        } else {
            use std::os::unix::process::CommandExt;
            let Some(program) = argv.first() else {
                tracing::error!("empty command");
                return 1;
            };
            std::process::Command::new(program)
                .args(&argv[1..])
                .env(real_cargo::SHIM_ACTIVE_ENV, "1")
                .exec()
                .to_string()
        };
        tracing::error!(error = %err, "failed to run command locally");
        1
    }
    fn spawn_touch(&self, dir: &Path) {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        // Fully detached: own process group, no stdio. It must never block the
        // build or change its exit code; failure to spawn is only debug noise.
        let result = Command::new(&self.exe)
            .arg("store-touch")
            .arg("--workspace")
            .arg(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
        match result {
            Ok(child) => {
                tracing::debug!(pid = child.id(), dir = %dir.display(), "spawned store-touch");
            }
            Err(e) => tracing::debug!(error = %e, "failed to spawn store-touch"),
        }
    }
    fn stdout(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes).and_then(|()| out.flush());
    }
    fn stderr(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut err = std::io::stderr().lock();
        let _ = err.write_all(bytes).and_then(|()| err.flush());
    }
    async fn cancel_signal(&self) {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            std::future::pending::<()>().await;
            return;
        };
        tokio::select! {
            _ = int.recv() => {}
            _ = term.recv() => {}
        }
    }
}

fn current_exe() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("cannot determine current executable")
}

/// Run `argv` (full, with program name) through the chain and return the exit code.
async fn run_chain(
    argv: Vec<String>,
    mode: Option<Mode>,
    cargo_policy: bool,
) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let mut cfg = rbs_config::load(&cwd).context("loading config")?;
    if let Some(m) = mode {
        cfg.policy.mode = m;
    }
    let exe = current_exe()?;
    let input = shim::ShimInput::from_env(cwd, argv, cfg.clone(), cargo_policy);
    let hooks = RealHooks { cfg, exe };
    Ok(shim::run(input, &hooks).await)
}

async fn shim_main(args: Vec<String>) -> anyhow::Result<i32> {
    if real_cargo::shim_active() {
        // Nested invocation (build script, server job): straight to the real cargo.
        let err = real_cargo::exec_real_cargo(&args);
        anyhow::bail!("{err}");
    }
    let mut argv = vec!["cargo".to_string()];
    argv.extend(args);
    run_chain(argv, None, true).await
}

async fn cli_main() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    match cli.cmd {
        Cmd::Status => {
            let cfg = rbs_config::load(&cwd).context("loading config")?;
            let remote = transport::SshTransport {
                host: cfg.remote.host.clone(),
                connect_timeout_ms: cfg.remote.connect_timeout_ms,
                remote_bin: cfg.remote.remote_bin.clone(),
            };
            let local = transport::UnixTransport {
                path: cfg.local_server.socket.clone(),
            };
            Ok(status::run(&cfg, Some(&remote), Some(&local)).await)
        }
        Cmd::Exec { mode, cmd } => run_chain(cmd, mode, false).await,
        Cmd::Server { socket, tokens } => {
            let cfg = rbs_config::load(&cwd).context("loading config")?;
            rbs_server::run_server(cfg, rbs_server::ServerOpts { socket, tokens }).await?;
            Ok(0)
        }
        Cmd::Proxy { socket } => {
            let cfg = rbs_config::load(&cwd).context("loading config")?;
            let socket = socket.unwrap_or(cfg.local_server.socket);
            rbs_server::run_proxy(socket).await?;
            Ok(0)
        }
        Cmd::Setup {
            role,
            remote_host,
            force,
        } => {
            rbs_setup::setup(rbs_setup::SetupOpts {
                role,
                remote_host,
                force,
                self_exe: current_exe()?,
            })?;
            Ok(0)
        }
        Cmd::Bootstrap {
            server,
            clients,
            dry_run,
            force,
        } => {
            let report = rbs_setup::bootstrap(rbs_setup::BootstrapOpts {
                server,
                clients,
                dry_run,
                force,
                self_exe: current_exe()?,
                workspace: cwd,
            })?;
            print!("{}", rbs_setup::render_report(&report));
            Ok(if report.ok() { 0 } else { 1 })
        }
        Cmd::Doctor { remote } => {
            let report = rbs_setup::doctor(rbs_setup::DoctorOpts { cwd, remote })?;
            let width = report
                .checks
                .iter()
                .map(|c| c.name.len())
                .max()
                .unwrap_or(0);
            for c in &report.checks {
                println!(
                    "{} {:<width$}  {}",
                    if c.ok { "ok  " } else { "FAIL" },
                    c.name,
                    c.detail,
                    width = width
                );
            }
            Ok(if report.ok() { 0 } else { 1 })
        }
        Cmd::StoreGc {
            dry_run,
            max_size_gib,
        } => {
            let report = rbs_store::run_gc(rbs_store::GcOpts {
                dry_run,
                max_size_gib_override: max_size_gib,
            })
            .await?;
            println!("{report}");
            Ok(0)
        }
        Cmd::StoreTouch { workspace, force } => {
            let report = rbs_store::run_touch(rbs_store::TouchOpts { workspace, force }).await?;
            println!("{report}");
            Ok(0)
        }
    }
}

fn invoked_as_cargo() -> bool {
    std::env::args_os()
        .next()
        .map(PathBuf::from)
        .and_then(|p| p.file_name().map(|f| f.to_os_string()))
        .is_some_and(|f| f == "cargo")
}

#[tokio::main]
async fn main() {
    init_logging();
    let result = if invoked_as_cargo() {
        shim_main(std::env::args().skip(1).collect()).await
    } else {
        cli_main().await
    };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
    }
}
