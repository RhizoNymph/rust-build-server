//! `rbs bootstrap`: provision every host of the fleet over ssh from one place.
//! See `docs/features/bootstrap.md`.
//!
//! One server, N clients. Every host walks the same ordered step list; a host
//! that fails never aborts the others, so the report is the whole truth about
//! the fleet after one run. Everything external goes through [`Runner`], so the
//! tests script the fleet without a network.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use rbs_config::Config;
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::paths::Paths;
use crate::runner::{Output, Runner};
use crate::{BootstrapReport, Role};

/// Remote locations. Relative to the remote `$HOME`: non-interactive ssh does
/// not expand `~` the way a login shell would, but it does start in `$HOME`.
const REMOTE_BIN_DIR: &str = ".local/bin";
const REMOTE_RBS: &str = ".local/bin/rbs";
const REMOTE_KACHE: &str = ".local/bin/kache";
const REMOTE_MINIO_ENV: &str = ".config/rbs/minio.env";
const CONNECT_TIMEOUT_SECS: u32 = 5;

/// Ordered step list; also the column order of the report table.
pub const STEPS: [&str; 7] = [
    "reach",
    "binaries",
    "secrets",
    "ssh-alias",
    "toolchain",
    "setup",
    "doctor",
];

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error(
        "no hosts to bootstrap: set `[bootstrap] server` / `clients` in ~/.config/rbs/config.toml, or pass --server / --clients"
    )]
    NoHosts,
}

#[derive(Debug, Clone)]
pub struct BootstrapOpts {
    /// Overrides `[bootstrap] server`.
    pub server: Option<String>,
    /// Overrides `[bootstrap] clients` when non-empty.
    pub clients: Vec<String>,
    /// Print the plan and execute nothing.
    pub dry_run: bool,
    /// Pass `--force` to the remote `rbs setup`.
    pub force: bool,
    /// The running `rbs` binary; pushed to every host.
    pub self_exe: PathBuf,
    /// Directory used to locate `rust-toolchain.toml` and the layered config.
    pub workspace: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    /// Nothing to do (already in the wanted state, or not applicable).
    Skipped,
    Failed,
}

impl StepStatus {
    pub fn label(self) -> &'static str {
        match self {
            StepStatus::Ok => "ok",
            StepStatus::Skipped => "skip",
            StepStatus::Failed => "FAIL",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: &'static str,
    pub status: StepStatus,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostReport {
    pub host: String,
    pub role: Role,
    /// Steps actually attempted, in order. A host that is unreachable has only
    /// `reach`; everything after it was never tried.
    pub steps: Vec<Step>,
    /// Operator-actionable notes. Never contains a secret value.
    pub notes: Vec<String>,
}

impl HostReport {
    fn new(host: &str, role: Role) -> Self {
        Self {
            host: host.to_string(),
            role,
            steps: Vec::new(),
            notes: Vec::new(),
        }
    }

    pub fn ok(&self) -> bool {
        self.steps.iter().all(|s| s.status != StepStatus::Failed)
    }

    pub fn status(&self, step: &str) -> Option<StepStatus> {
        self.steps.iter().find(|s| s.name == step).map(|s| s.status)
    }

    fn step(&mut self, name: &'static str, status: StepStatus, detail: impl Into<String>) {
        let detail = detail.into();
        debug!(host = %self.host, step = name, status = status.label(), detail = %detail, "bootstrap step");
        self.steps.push(Step {
            name,
            status,
            detail,
        });
    }

    fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
}

// ------------------------------------------------------------- pure helpers

/// The hosts to visit, server first, each host exactly once. A host named as
/// both server and client keeps the server role.
pub fn plan_hosts(server: &str, clients: &[String]) -> Vec<(String, Role)> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut hosts = Vec::new();
    let server = server.trim();
    if !server.is_empty() {
        seen.insert(server.to_string());
        hosts.push((server.to_string(), Role::Server));
    }
    for client in clients {
        let client = client.trim();
        if client.is_empty() || !seen.insert(client.to_string()) {
            continue;
        }
        hosts.push((client.to_string(), Role::Client));
    }
    hosts
}

/// `HostName` of the first `Host` block naming `host` in an ssh config.
/// Keywords are case-insensitive, a `Host` line may list several aliases, and
/// wildcard patterns are never treated as a match (they carry no useful IP).
pub fn ssh_config_hostname(text: &str, host: &str) -> Option<String> {
    let mut in_block = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, rest)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else {
            continue;
        };
        let value = rest.trim_start_matches(['=', ' ', '\t']).trim();
        match key.trim().to_ascii_lowercase().as_str() {
            "host" => in_block = value.split_whitespace().any(|pattern| pattern == host),
            "hostname" if in_block && !value.is_empty() => return Some(value.to_string()),
            _ => {}
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct ToolchainFile {
    toolchain: ToolchainSection,
}

#[derive(Debug, Deserialize)]
struct ToolchainSection {
    channel: Option<String>,
}

/// `[toolchain] channel` of a `rust-toolchain.toml`.
pub fn parse_toolchain_channel(text: &str) -> Option<String> {
    let parsed: ToolchainFile = toml::from_str(text).ok()?;
    parsed
        .toolchain
        .channel
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

/// Nearest `rust-toolchain.toml` at or above `start`.
pub fn find_toolchain_file(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join("rust-toolchain.toml"))
        .find(|p| p.is_file())
}

/// Is `channel` present in `rustup toolchain list` output? Entries are
/// `<channel>-<host triple>`, optionally followed by `(default)`.
pub fn toolchain_installed(list: &str, channel: &str) -> bool {
    let prefix = format!("{channel}-");
    list.lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|name| name == channel || name.starts_with(&prefix))
}

/// Names of the failing checks in `rbs doctor` output.
pub fn doctor_failed_checks(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter_map(|l| l.strip_prefix("FAIL"))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(str::to_string)
        .collect()
}

// ------------------------------------------------------------------ running

/// Run a command; a spawn failure reads like any other failure.
fn exec(runner: &dyn Runner, program: &str, args: &[&str]) -> Result<Output, String> {
    runner.run(program, args).map_err(|e| e.to_string())
}

/// Run a command and require success, returning its stdout.
fn run_ok(runner: &dyn Runner, program: &str, args: &[&str]) -> Result<String, String> {
    let out = exec(runner, program, args)?;
    if out.success() {
        Ok(out.stdout)
    } else {
        Err(out.failure_detail())
    }
}

struct Fleet<'a> {
    runner: &'a dyn Runner,
    paths: &'a Paths,
    cfg: &'a Config,
    opts: &'a BootstrapOpts,
    server: String,
    /// Resolved once for the run: `Ok(channel)` or `Err(actionable note)`.
    toolchain: Result<String, String>,
}

impl Fleet<'_> {
    /// Local `kache` binary to push (`[bootstrap] kache_bin`, else `~/.local/bin/kache`).
    fn kache_source(&self) -> PathBuf {
        let configured = self.cfg.bootstrap.kache_bin.trim();
        match configured.strip_prefix("~/") {
            Some(rest) => self.paths.home.join(rest),
            None if configured.is_empty() => self.paths.home.join(REMOTE_KACHE),
            None => PathBuf::from(configured),
        }
    }

    fn run_host(&self, host: &str, role: Role) -> HostReport {
        let mut rep = HostReport::new(host, role);
        info!(host, role = role.name(), "bootstrapping host");
        if !self.reach(host, &mut rep) {
            warn!(host, "unreachable over ssh; skipping the remaining steps");
            return rep;
        }
        self.binaries(host, &mut rep);
        self.secrets(host, &mut rep);
        self.ssh_alias(host, role, &mut rep);
        self.toolchain(host, &mut rep);
        self.setup(host, role, &mut rep);
        self.doctor(host, role, &mut rep);
        rep
    }

    /// Step 1. Everything downstream needs ssh, so a failure here ends the host.
    fn reach(&self, host: &str, rep: &mut HostReport) -> bool {
        let timeout = format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}");
        let args = ["-o", "BatchMode=yes", "-o", &timeout, host, "true"];
        match run_ok(self.runner, "ssh", &args) {
            Ok(_) => {
                rep.step("reach", StepStatus::Ok, "reachable");
                true
            }
            Err(e) => {
                rep.step("reach", StepStatus::Failed, format!("ssh {host}: {e}"));
                rep.note(format!(
                    "{host} is unreachable with BatchMode ssh; check `ssh {host} true`, the host's ssh alias and its authorized_keys"
                ));
                false
            }
        }
    }

    /// Step 2. Binaries are staged as `<path>.new` and `mv -f`'d into place:
    /// a running server holds its own binary open, so overwriting in place
    /// fails (ETXTBSY) or corrupts the running process.
    fn binaries(&self, host: &str, rep: &mut HostReport) {
        if let Err(e) = run_ok(self.runner, "ssh", &[host, "mkdir", "-p", REMOTE_BIN_DIR]) {
            rep.step(
                "binaries",
                StepStatus::Failed,
                format!("mkdir -p {REMOTE_BIN_DIR}: {e}"),
            );
            return;
        }
        let exe = self.opts.self_exe.clone();
        if let Err(e) = self.push_binary(host, &exe, REMOTE_RBS) {
            rep.step("binaries", StepStatus::Failed, e);
            return;
        }
        let kache = self.kache_source();
        if !kache.is_file() {
            // kache may well be installed on the host already; `doctor` decides.
            rep.note(format!(
                "no local kache binary at {}; nothing pushed to {host} — install kache there by hand (or set `[bootstrap] kache_bin`) if doctor reports it missing",
                kache.display()
            ));
            rep.step(
                "binaries",
                StepStatus::Ok,
                "rbs pushed; kache skipped (no local binary)",
            );
            return;
        }
        if let Err(e) = self.push_binary(host, &kache, REMOTE_KACHE) {
            rep.step("binaries", StepStatus::Failed, e);
            return;
        }
        rep.step("binaries", StepStatus::Ok, "rbs + kache pushed");
    }

    fn push_binary(&self, host: &str, local: &Path, remote: &str) -> Result<(), String> {
        let staged = format!("{remote}.new");
        let src = local.display().to_string();
        let dest = format!("{host}:{staged}");
        run_ok(self.runner, "scp", &[&src, &dest]).map_err(|e| format!("scp {src} {dest}: {e}"))?;
        run_ok(self.runner, "ssh", &[host, "mv", "-f", &staged, remote])
            .map_err(|e| format!("mv {staged} {remote} on {host}: {e}"))?;
        info!(host, remote, "pushed binary");
        Ok(())
    }

    /// Step 3. Credentials travel only when `push_secrets` is on; otherwise the
    /// host is merely checked and the operator is told the exact command.
    fn secrets(&self, host: &str, rep: &mut HostReport) {
        let local = self.paths.minio_env();
        if !self.cfg.bootstrap.push_secrets {
            let probe = format!("test -f {REMOTE_MINIO_ENV}");
            if run_ok(self.runner, "ssh", &[host, &probe]).is_ok() {
                rep.step(
                    "secrets",
                    StepStatus::Skipped,
                    "present on host (push_secrets = false)",
                );
            } else {
                rep.note(format!(
                    "{host} has no {REMOTE_MINIO_ENV}; copy it yourself with `scp {} {host}:{REMOTE_MINIO_ENV}` or set `[bootstrap] push_secrets = true` — until then the kache step of setup on {host} fails",
                    local.display()
                ));
                rep.step(
                    "secrets",
                    StepStatus::Skipped,
                    "missing on host (push_secrets = false)",
                );
            }
            return;
        }
        if !local.is_file() {
            rep.note(format!(
                "push_secrets is on but {} does not exist locally; nothing copied to {host}",
                local.display()
            ));
            rep.step("secrets", StepStatus::Skipped, "no local minio.env");
            return;
        }
        let src = local.display().to_string();
        let dest = format!("{host}:{REMOTE_MINIO_ENV}");
        let push = || -> Result<(), String> {
            run_ok(self.runner, "ssh", &[host, "mkdir", "-p", ".config/rbs"])?;
            run_ok(self.runner, "scp", &[&src, &dest])?;
            run_ok(
                self.runner,
                "ssh",
                &[host, "chmod", "600", REMOTE_MINIO_ENV],
            )?;
            Ok(())
        };
        match push() {
            // The file's contents are never read here, so they cannot be logged.
            Ok(()) => rep.step("secrets", StepStatus::Ok, "minio.env copied, mode 600"),
            Err(e) => rep.step("secrets", StepStatus::Failed, e),
        }
    }

    /// Step 4. Clients need an ssh alias for the server before they can use it
    /// as a backend. The server itself needs no alias to itself.
    fn ssh_alias(&self, host: &str, role: Role, rep: &mut HostReport) {
        if role == Role::Server {
            rep.step("ssh-alias", StepStatus::Skipped, "not applicable (server)");
            return;
        }
        if self.server.is_empty() {
            rep.step("ssh-alias", StepStatus::Skipped, "no server configured");
            return;
        }
        let server = self.server.as_str();
        let probe = format!("test -f .ssh/config && grep -q 'Host {server}' .ssh/config");
        if run_ok(self.runner, "ssh", &[host, &probe]).is_ok() {
            rep.step(
                "ssh-alias",
                StepStatus::Skipped,
                format!("`Host {server}` already in {host}:.ssh/config"),
            );
            return;
        }
        let local_config = self.paths.home.join(".ssh/config");
        let hostname = std::fs::read_to_string(&local_config)
            .ok()
            .and_then(|text| ssh_config_hostname(&text, server));
        let Some(hostname) = hostname else {
            rep.note(format!(
                "no HostName for `{server}` in {}; add a `Host {server}` block to {host}:.ssh/config by hand so it can reach the server",
                local_config.display()
            ));
            rep.step(
                "ssh-alias",
                StepStatus::Skipped,
                "no local HostName to copy",
            );
            return;
        };
        // `printf '%s\n' …` keeps the quoting flat: no heredoc, one ssh command.
        let append = format!(
            "mkdir -p .ssh && chmod 700 .ssh && printf '%s\\n' 'Host {server}' '  HostName {hostname}' '  StrictHostKeyChecking accept-new' >> .ssh/config"
        );
        match run_ok(self.runner, "ssh", &[host, &append]) {
            Ok(_) => rep.step(
                "ssh-alias",
                StepStatus::Ok,
                format!("appended `Host {server}` -> {hostname}"),
            ),
            Err(e) => rep.step("ssh-alias", StepStatus::Failed, e),
        }
    }

    /// Step 5. The fleet must agree on the toolchain: cache keys include the
    /// rustc commit hash, so a mismatched host cannot reuse anything.
    fn toolchain(&self, host: &str, rep: &mut HostReport) {
        let channel = match &self.toolchain {
            Ok(channel) => channel.clone(),
            Err(note) => {
                rep.note(note.clone());
                rep.step("toolchain", StepStatus::Skipped, "channel unknown");
                return;
            }
        };
        let list = match run_ok(self.runner, "ssh", &[host, "rustup toolchain list"]) {
            Ok(list) => list,
            Err(e) => {
                rep.note(format!(
                    "rustup is not usable on {host} ({e}); install it with `ssh {host} \"curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal\"`"
                ));
                rep.step("toolchain", StepStatus::Skipped, "rustup unavailable");
                return;
            }
        };
        if toolchain_installed(&list, &channel) {
            rep.step(
                "toolchain",
                StepStatus::Skipped,
                format!("{channel} already installed"),
            );
            return;
        }
        let install = format!("rustup toolchain install {channel} --profile minimal");
        match run_ok(self.runner, "ssh", &[host, &install]) {
            Ok(_) => {
                info!(host, channel, "installed toolchain");
                rep.step("toolchain", StepStatus::Ok, format!("installed {channel}"));
            }
            Err(e) => rep.step("toolchain", StepStatus::Failed, format!("{install}: {e}")),
        }
    }

    /// Step 6. `rbs setup` by explicit path: a non-interactive ssh PATH has no
    /// `~/.local/bin`.
    fn setup(&self, host: &str, role: Role, rep: &mut HostReport) {
        let mut args = vec![host, REMOTE_RBS, "setup", "--role", role.name()];
        if self.opts.force {
            args.push("--force");
        }
        match exec(self.runner, "ssh", &args) {
            Ok(out) if out.success() => {
                debug!(host, stdout = %out.stdout.trim(), "remote setup output");
                rep.step(
                    "setup",
                    StepStatus::Ok,
                    format!("rbs setup --role {}", role.name()),
                );
            }
            Ok(out) => rep.step("setup", StepStatus::Failed, out.failure_detail()),
            Err(e) => rep.step("setup", StepStatus::Failed, e),
        }
    }

    /// Step 7. Clients also verify their link to the server (`--remote`).
    fn doctor(&self, host: &str, role: Role, rep: &mut HostReport) {
        let mut args = vec![host, REMOTE_RBS, "doctor"];
        if role == Role::Client {
            args.push("--remote");
        }
        match exec(self.runner, "ssh", &args) {
            Ok(out) if out.success() => {
                rep.step("doctor", StepStatus::Ok, "all checks passed");
            }
            Ok(out) => {
                let failed = doctor_failed_checks(&out.stdout);
                let detail = if failed.is_empty() {
                    out.failure_detail()
                } else {
                    format!("failed checks: {}", failed.join(", "))
                };
                rep.step("doctor", StepStatus::Failed, detail);
            }
            Err(e) => rep.step("doctor", StepStatus::Failed, e),
        }
    }

    /// What the run *would* do, per host. Descriptive on purpose: `--dry-run`
    /// must not execute anything, not even a probe.
    fn plan(&self, host: &str, role: Role) -> Vec<(&'static str, String)> {
        let force = if self.opts.force { " --force" } else { "" };
        let remote = if role == Role::Client {
            " --remote"
        } else {
            ""
        };
        vec![
            (
                "reach",
                format!(
                    "ssh -o BatchMode=yes -o ConnectTimeout={CONNECT_TIMEOUT_SECS} {host} true"
                ),
            ),
            (
                "binaries",
                format!(
                    "push {} -> {host}:{REMOTE_RBS} and {} -> {host}:{REMOTE_KACHE} (staged .new, then mv -f)",
                    self.opts.self_exe.display(),
                    self.kache_source().display()
                ),
            ),
            (
                "secrets",
                if self.cfg.bootstrap.push_secrets {
                    format!(
                        "copy {} -> {host}:{REMOTE_MINIO_ENV} (mode 600)",
                        self.paths.minio_env().display()
                    )
                } else {
                    format!("check {host}:{REMOTE_MINIO_ENV} exists (push_secrets = false)")
                },
            ),
            (
                "ssh-alias",
                if role == Role::Server {
                    "not applicable (server)".to_string()
                } else {
                    format!("ensure `Host {}` in {host}:.ssh/config", self.server)
                },
            ),
            (
                "toolchain",
                match &self.toolchain {
                    Ok(channel) => format!("ensure rustup toolchain {channel} on {host}"),
                    Err(note) => format!("skipped: {note}"),
                },
            ),
            (
                "setup",
                format!(
                    "ssh {host} {REMOTE_RBS} setup --role {}{force}",
                    role.name()
                ),
            ),
            ("doctor", format!("ssh {host} {REMOTE_RBS} doctor{remote}")),
        ]
    }

    fn dry_run(&self, hosts: &[(String, Role)]) -> BootstrapReport {
        println!("dry run: nothing is executed");
        let mut report = BootstrapReport::default();
        for (host, role) in hosts {
            println!("\n{host} ({}):", role.name());
            let mut rep = HostReport::new(host, *role);
            for (name, action) in self.plan(host, *role) {
                println!("  {name:<9}  {action}");
                rep.step(name, StepStatus::Skipped, action);
            }
            report.hosts.push(rep);
        }
        report
    }
}

/// The toolchain channel the whole fleet should have: `[bootstrap] toolchain`,
/// else the workspace's `rust-toolchain.toml` pin. `Err` is an operator note,
/// not a failure: an unknown channel only means the step is skipped.
fn resolve_channel(cfg: &Config, opts: &BootstrapOpts) -> Result<String, String> {
    let configured = cfg.bootstrap.toolchain.trim();
    if !configured.is_empty() {
        return Ok(configured.to_string());
    }
    let Some(file) = find_toolchain_file(&opts.workspace) else {
        return Err(format!(
            "no rust-toolchain.toml at or above {}; set `[bootstrap] toolchain` or run bootstrap from the workspace to pin the fleet's toolchain",
            opts.workspace.display()
        ));
    };
    let text = std::fs::read_to_string(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    parse_toolchain_channel(&text).ok_or_else(|| {
        format!(
            "{} has no `[toolchain] channel`; set `[bootstrap] toolchain` instead",
            file.display()
        )
    })
}

/// Testable core of [`crate::bootstrap`].
pub fn bootstrap_with(
    runner: &dyn Runner,
    paths: &Paths,
    cfg: &Config,
    opts: &BootstrapOpts,
) -> Result<BootstrapReport, BootstrapError> {
    let server = opts
        .server
        .clone()
        .unwrap_or_else(|| cfg.bootstrap.server.clone())
        .trim()
        .to_string();
    let clients = if opts.clients.is_empty() {
        cfg.bootstrap.clients.clone()
    } else {
        opts.clients.clone()
    };
    let hosts = plan_hosts(&server, &clients);
    if hosts.is_empty() {
        return Err(BootstrapError::NoHosts);
    }
    let fleet = Fleet {
        runner,
        paths,
        cfg,
        opts,
        toolchain: resolve_channel(cfg, opts),
        server,
    };
    if opts.dry_run {
        return Ok(fleet.dry_run(&hosts));
    }
    info!(hosts = hosts.len(), "rbs bootstrap");
    let mut report = BootstrapReport::default();
    for (host, role) in &hosts {
        report.hosts.push(fleet.run_host(host, *role));
    }
    Ok(report)
}

/// Per-host table, the details of every failed step, the notes, and a summary.
pub fn render_report(report: &BootstrapReport) -> String {
    let host_w = report
        .hosts
        .iter()
        .map(|h| h.host.len())
        .chain(std::iter::once("HOST".len()))
        .max()
        .unwrap_or(4);
    let role_w = 6;
    let row = |host: &str, role: &str, cell: &dyn Fn(&str) -> &str| {
        let mut line = format!("{host:<host_w$}  {role:<role_w$}");
        for step in STEPS {
            line.push_str(&format!("  {:<w$}", cell(step), w = step.len()));
        }
        format!("{}\n", line.trim_end())
    };
    let mut out = row("HOST", "ROLE", &|step| step);
    for host in &report.hosts {
        out.push_str(&row(&host.host, host.role.name(), &|step| {
            host.status(step).map_or("-", StepStatus::label)
        }));
    }
    let failures: Vec<String> = report
        .hosts
        .iter()
        .flat_map(|h| {
            h.steps
                .iter()
                .filter(|s| s.status == StepStatus::Failed)
                .map(move |s| format!("  {} {}: {}", h.host, s.name, s.detail.trim()))
        })
        .collect();
    if !failures.is_empty() {
        out.push_str(&format!("\nfailures:\n{}\n", failures.join("\n")));
    }
    let notes: Vec<String> = report
        .hosts
        .iter()
        .flat_map(|h| h.notes.iter().map(move |n| format!("  {}: {n}", h.host)))
        .collect();
    if !notes.is_empty() {
        out.push_str(&format!("\nnotes:\n{}\n", notes.join("\n")));
    }
    let failed = report.hosts.iter().filter(|h| !h.ok()).count();
    out.push_str(&format!(
        "\n{} hosts: {} ok, {failed} failed\n",
        report.hosts.len(),
        report.hosts.len() - failed
    ));
    out
}
