//! `rbs setup` / `rbs doctor`. See `docs/features/setup.md`.
//!
//! Public API consumed by the `rbs` binary (crates/rbs-client). Keep these
//! signatures stable; the client is developed against them in parallel.

mod doctor;
pub mod paths;
pub mod runner;
mod setup;
#[cfg(test)]
mod testing;

#[cfg(test)]
mod doctor_tests;
#[cfg(test)]
mod setup_tests;

use std::path::PathBuf;

pub use doctor::{REQUIRED_BINARIES, doctor_with};
pub use paths::{Paths, PathsError};
pub use runner::{Output, Runner, RunnerError, SystemRunner};
pub use setup::{
    KACHE_CONFIG_DEFAULT, STORE_GC_TIMER, SetupError, generate_config, render_store_gc_service,
    render_unit, setup_with,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Laptop,
    Node0,
}

impl std::str::FromStr for Role {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "laptop" => Ok(Role::Laptop),
            "node0" => Ok(Role::Node0),
            other => Err(format!("unknown role `{other}` (expected laptop|node0)")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SetupOpts {
    pub role: Role,
    /// When set (laptop role), also provision this ssh host with role node0.
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
