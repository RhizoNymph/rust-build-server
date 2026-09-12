//! `rbs setup` / `rbs doctor`. See `docs/features/setup.md`.
//!
//! Public API consumed by the `rbs` binary (crates/rbs-client). Keep these
//! signatures stable; the client is developed against them in parallel.

mod bootstrap;
mod doctor;
pub mod paths;
pub mod runner;
mod setup;
#[cfg(test)]
mod testing;

#[cfg(test)]
mod bootstrap_tests;
#[cfg(test)]
mod doctor_tests;
#[cfg(test)]
mod setup_tests;

use std::path::PathBuf;

pub use bootstrap::{
    BootstrapError, BootstrapOpts, HostReport, STEPS, Step, StepStatus, bootstrap_with,
    doctor_failed_checks, find_toolchain_file, parse_toolchain_channel, plan_hosts, render_report,
    ssh_config_hostname, toolchain_installed,
};
pub use doctor::{REQUIRED_BINARIES, doctor_with};
pub use paths::{Paths, PathsError};
pub use runner::{Output, Runner, RunnerError, SystemRunner};
pub use setup::{
    KACHE_CONFIG_DEFAULT, STORE_GC_TIMER, SetupError, generate_config, render_store_gc_service,
    render_unit, setup_with,
};

/// What a host does in the fleet. Topology, not hardware: one `Server`, N
/// `Client`s. `laptop` / `node0` are accepted as legacy aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

impl Role {
    /// Canonical name as used on the command line and in generated config.
    pub fn name(self) -> &'static str {
        match self {
            Role::Client => "client",
            Role::Server => "server",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            // Legacy aliases: `laptop` was the client, `node0` the server.
            "client" | "laptop" => Ok(Role::Client),
            "server" | "node0" => Ok(Role::Server),
            other => Err(format!(
                "unknown role `{other}` (expected client|server; laptop|node0 accepted as aliases)"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupOpts {
    pub role: Role,
    /// When set (client role), also provision this ssh host with role server.
    pub remote_host: Option<String>,
    /// Overwrite existing config files instead of printing a diff.
    pub force: bool,
    /// Path of the running `rbs` binary (for shim hardlink and scp to remote).
    pub self_exe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct DoctorOpts {
    pub cwd: PathBuf,
    /// Probe the remote host too (ssh round trip).
    pub remote: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    pub fn ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// Fleet-wide result of `rbs bootstrap`: one entry per host, in visit order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootstrapReport {
    pub hosts: Vec<HostReport>,
}

impl BootstrapReport {
    /// True only when every host got through every attempted step.
    pub fn ok(&self) -> bool {
        self.hosts.iter().all(HostReport::ok)
    }
}

pub fn setup(opts: SetupOpts) -> anyhow::Result<()> {
    let paths = Paths::from_env(Some(opts.self_exe.clone()))?;
    setup_with(&SystemRunner, &paths, &opts)?;
    Ok(())
}

pub fn doctor(opts: DoctorOpts) -> anyhow::Result<DoctorReport> {
    let paths = Paths::from_env(None)?;
    let cfg = rbs_config::load(&opts.cwd)?;
    Ok(doctor_with(&SystemRunner, &paths, &cfg, &opts)?)
}

pub fn bootstrap(opts: BootstrapOpts) -> anyhow::Result<BootstrapReport> {
    let paths = Paths::from_env(Some(opts.self_exe.clone()))?;
    let cfg = rbs_config::load(&opts.workspace)?;
    Ok(bootstrap_with(&SystemRunner, &paths, &cfg, &opts)?)
}
