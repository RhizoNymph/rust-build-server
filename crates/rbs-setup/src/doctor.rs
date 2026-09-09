//! `rbs doctor`: health checks. See `docs/features/setup.md`.

use std::path::Path;
use std::time::Instant;

use rbs_config::Config;
use rbs_toolchain::{ToolchainFingerprint, parse_rustc_vv};
use tracing::debug;

use crate::paths::Paths;
use crate::runner::{Output, Runner, RunnerError};
use crate::setup::kache_stats_has_remote;
use crate::{Check, DoctorOpts, DoctorReport};

pub const REQUIRED_BINARIES: [&str; 7] = [
    "rbs",
    "cargo",
    "rustup",
    "kache",
    "rsync",
    "ssh",
    "systemd-run",
];

fn check(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

/// `kache daemon status` reports e.g. `running (pid N)` or `not running`.
fn daemon_is_running(status: &str) -> bool {
    let s = status.to_lowercase();
    s.contains("running") && !s.contains("not running")
}

/// Collapse a command result into `Ok(stdout)` or `Err(detail)`; never panics.
fn outcome(res: Result<Output, RunnerError>) -> Result<String, String> {
    match res {
        Ok(out) if out.success() => Ok(out.stdout),
        Ok(out) => Err(out.failure_detail()),
        Err(e) => Err(e.to_string()),
    }
}

fn run(
    runner: &dyn Runner,
    cwd: Option<&Path>,
    program: &str,
    args: &[&str],
) -> Result<String, String> {
    debug!(program, ?args, "doctor: run");
    outcome(runner.run_in(cwd, program, args))
}

/// Fingerprint the toolchain active in `cwd` through the runner (mirrors
/// `rbs_toolchain::fingerprint` but stays hermetic).
fn local_fingerprint(runner: &dyn Runner, cwd: &Path) -> Result<ToolchainFingerprint, String> {
    let vv =
        parse_rustc_vv(&run(runner, Some(cwd), "rustc", &["-vV"])?).map_err(|e| e.to_string())?;
    let cargo_version = run(runner, Some(cwd), "cargo", &["-V"])?.trim().to_string();
    let toolchain_name = run(runner, Some(cwd), "rustup", &["show", "active-toolchain"])
        .map(|s| s.split_whitespace().next().unwrap_or_default().to_string())
        .unwrap_or_default();
    Ok(ToolchainFingerprint {
        rustc_commit: vv.commit_hash,
        rustc_version: vv.release,
        host: vv.host,
        cargo_version,
        toolchain_name,
    })
}

fn remote_fingerprint(
    runner: &dyn Runner,
    host: &str,
    cwd: &Path,
) -> Result<ToolchainFingerprint, String> {
    let cmd = format!("cd {} && rustc -vV", cwd.display());
    let vv =
        parse_rustc_vv(&run(runner, None, "ssh", &[host, &cmd])?).map_err(|e| e.to_string())?;
    Ok(ToolchainFingerprint {
        rustc_commit: vv.commit_hash,
        rustc_version: vv.release,
        host: vv.host,
        cargo_version: String::new(),
        toolchain_name: String::new(),
    })
}

fn remote_checks(
    runner: &dyn Runner,
    host: &str,
    remote_bin: &str,
    cwd: &Path,
    local: Option<&ToolchainFingerprint>,
    checks: &mut Vec<Check>,
) {
    let started = Instant::now();
    let ssh = run(
        runner,
        None,
        "ssh",
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            host,
            "true",
        ],
    );
    let rtt = started.elapsed();
    match ssh {
        Ok(_) => checks.push(check(
            "remote-ssh",
            true,
            format!("{host} reachable, rtt {} ms", rtt.as_millis()),
        )),
        Err(e) => {
            checks.push(check("remote-ssh", false, format!("ssh {host}: {e}")));
            checks.push(check("remote-rbs", false, "skipped: ssh unreachable"));
            checks.push(check("remote-toolchain", false, "skipped: ssh unreachable"));
            return;
        }
    }
    // Explicit path: non-interactive ssh shells usually lack ~/.local/bin on PATH.
    match run(runner, None, "ssh", &[host, remote_bin, "--version"]) {
        Ok(v) => checks.push(check("remote-rbs", true, v.trim())),
        Err(e) => checks.push(check(
            "remote-rbs",
            false,
            format!("{e} (run `rbs setup --remote-host {host}`)"),
        )),
    }
    let Some(local) = local else {
        checks.push(check(
            "remote-toolchain",
            false,
            "skipped: local toolchain unknown",
        ));
        return;
    };
    match remote_fingerprint(runner, host, cwd) {
        Ok(remote) if local.compatible_with(&remote) => {
            checks.push(check("remote-toolchain", true, format!("{host}: {remote}")));
        }
        Ok(remote) => checks.push(check(
            "remote-toolchain",
            false,
            format!(
                "mismatch for {}\n    local:  {local}\n    {host}: {remote}\n    pin with rust-toolchain.toml and `rustup toolchain install {}` on {host}",
                cwd.display(),
                local.rustc_version
            ),
        )),
        Err(e) => checks.push(check("remote-toolchain", false, format!("{host}: {e}"))),
    }
}

/// Testable core of [`crate::doctor`]. `cfg` must already have paths expanded.
pub fn doctor_with(
    runner: &dyn Runner,
    paths: &Paths,
    cfg: &Config,
    opts: &DoctorOpts,
) -> Result<DoctorReport, RunnerError> {
    let mut checks = Vec::new();

    for bin in REQUIRED_BINARIES {
        match paths.find_on_path(bin) {
            Some(p) => checks.push(check(
                format!("binary:{bin}"),
                true,
                p.display().to_string(),
            )),
            None => checks.push(check(
                format!("binary:{bin}"),
                false,
                format!("{bin} not found on PATH"),
            )),
        }
    }

    let kache = paths.kache_bin();
    match run(runner, None, &kache, &["stats"]) {
        Ok(out) if kache_stats_has_remote(&out) => {
            let line = out
                .lines()
                .find(|l| l.starts_with("Remote:"))
                .unwrap_or_default()
                .trim();
            checks.push(check("kache-remote", true, line));
        }
        Ok(out) => checks.push(check(
            "kache-remote",
            false,
            format!(
                "`kache stats` shows no s3 remote; check {}\n{}",
                paths.kache_config().display(),
                out.trim()
            ),
        )),
        Err(e) => checks.push(check("kache-remote", false, e)),
    }

    match run(runner, None, &kache, &["daemon", "status"]) {
        Ok(out) if daemon_is_running(&out) => checks.push(check("kache-daemon", true, out.trim())),
        Ok(out) => checks.push(check(
            "kache-daemon",
            false,
            format!("not running: {}", out.trim()),
        )),
        Err(e) => checks.push(check("kache-daemon", false, e)),
    }

    let sock = &cfg.local_server.socket;
    checks.push(if sock.exists() {
        check("server-socket", true, sock.display().to_string())
    } else {
        check(
            "server-socket",
            false,
            format!(
                "{} missing (systemctl --user status rbs-server)",
                sock.display()
            ),
        )
    });

    let shim_dir = paths.shim_dir();
    checks.push(match paths.find_on_path("cargo") {
        Some(p) if p.starts_with(&shim_dir) => check("shim-precedence", true, p.display().to_string()),
        Some(p) => check(
            "shim-precedence",
            false,
            format!(
                "first cargo on PATH is {} — add: export PATH=\"$HOME/.local/share/rbs/shim:$PATH\"",
                p.display()
            ),
        ),
        None => check("shim-precedence", false, "no cargo on PATH"),
    });

    let local = match local_fingerprint(runner, &opts.cwd) {
        Ok(fp) => {
            checks.push(check("toolchain", true, fp.to_string()));
            Some(fp)
        }
        Err(e) => {
            checks.push(check(
                "toolchain",
                false,
                format!("{}: {e}", opts.cwd.display()),
            ));
            None
        }
    };

    if opts.remote {
        remote_checks(
            runner,
            &cfg.remote.host,
            &cfg.remote.remote_bin,
            &opts.cwd,
            local.as_ref(),
            &mut checks,
        );
    }

    Ok(DoctorReport { checks })
}
