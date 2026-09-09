use std::path::Path;

use rbs_config::Config;

use crate::doctor::{REQUIRED_BINARIES, doctor_with};
use crate::testing::{FakeRunner, TempHome};
use crate::{DoctorOpts, DoctorReport};

const VV_A: &str = "rustc 1.95.0 (59807616e 2026-04-14)\nbinary: rustc\ncommit-hash: 59807616e0a1b2c3d4e5f60718293a4b5c6d7e8f\ncommit-date: 2026-04-14\nhost: x86_64-unknown-linux-gnu\nrelease: 1.95.0\nLLVM version: 21.1.0\n";
const VV_B: &str = "rustc 1.96.0 (abcdef123 2026-06-01)\nbinary: rustc\ncommit-hash: abcdef1234567890\ncommit-date: 2026-06-01\nhost: x86_64-unknown-linux-gnu\nrelease: 1.96.0\nLLVM version: 21.1.0\n";
const KACHE_STATS_OK: &str = "Local:      /home/u/.cache/kache\nRemote:     s3://kache/artifacts\n";

/// A home where every required binary exists, the shim is first on PATH, the
/// server socket is bound, and an rbs config is present.
struct Green {
    th: TempHome,
    cfg: Config,
    _listener: std::os::unix::net::UnixListener,
}

fn green() -> Green {
    let mut th = TempHome::new();
    let shim = th.paths.shim_dir();
    th.fake_bin(&shim, "cargo");
    let bin = th.paths.home.join("bin");
    for b in REQUIRED_BINARIES {
        th.fake_bin(&bin, b);
    }
    th.paths.path_dirs = vec![shim, bin];
    let mut cfg = Config::default();
    cfg.local_server.socket = th.dir.path().join("s.sock");
    let listener = std::os::unix::net::UnixListener::bind(&cfg.local_server.socket).expect("bind");
    Green {
        th,
        cfg,
        _listener: listener,
    }
}

fn remote_cmd(cwd: &Path) -> String {
    format!("cd {} && rustc -vV", cwd.display())
}

fn green_runner(cwd: &Path) -> FakeRunner {
    FakeRunner::new()
        .ok_args("kache", &["stats"], KACHE_STATS_OK)
        .ok_args(
            "kache",
            &["daemon", "status"],
            "kache daemon: running (pid 1)\n",
        )
        .ok_args("rustc", &["-vV"], VV_A)
        .ok_args("cargo", &["-V"], "cargo 1.95.0 (abc 2026-04-01)\n")
        .ok_args(
            "rustup",
            &["show", "active-toolchain"],
            "1.95.0-x86_64-unknown-linux-gnu (default)\n",
        )
        .ok_args(
            "ssh",
            &[
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=3",
                "node0",
                "true",
            ],
            "",
        )
        .ok_args(
            "ssh",
            &["node0", ".local/bin/rbs", "--version"],
            "rbs 0.1.0\n",
        )
        .ok_args("ssh", &["node0", &remote_cmd(cwd)], VV_A)
}

fn check<'a>(r: &'a DoctorReport, name: &str) -> &'a crate::Check {
    r.checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check named {name}: {:?}", r.checks))
}

fn local_opts(th: &TempHome) -> DoctorOpts {
    DoctorOpts {
        cwd: th.paths.home.clone(),
        remote: false,
    }
}

#[test]
fn all_green_local() {
    let g = green();
    let r = green_runner(&g.th.paths.home);
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &local_opts(&g.th)).expect("doctor");
    assert!(rep.ok(), "{:?}", rep.checks);
    for b in REQUIRED_BINARIES {
        assert!(check(&rep, &format!("binary:{b}")).ok);
    }
    assert!(check(&rep, "kache-remote").ok);
    assert!(check(&rep, "kache-daemon").ok);
    assert!(check(&rep, "server-socket").ok);
    assert!(check(&rep, "shim-precedence").ok);
    let tc = check(&rep, "toolchain");
    assert!(tc.ok);
    assert!(tc.detail.contains("rustc 1.95.0 (59807616e)"));
    assert!(rep.checks.iter().all(|c| !c.name.starts_with("remote")));
}

#[test]
fn all_green_remote() {
    let g = green();
    let r = green_runner(&g.th.paths.home);
    let mut o = local_opts(&g.th);
    o.remote = true;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    assert!(rep.ok(), "{:?}", rep.checks);
    assert!(check(&rep, "remote-ssh").detail.contains("ms"));
    assert!(check(&rep, "remote-rbs").detail.contains("rbs 0.1.0"));
    assert!(check(&rep, "remote-toolchain").ok);
}

#[test]
fn missing_binary_fails_only_that_check() {
    let g = green();
    std::fs::remove_file(g.th.paths.home.join("bin/rsync")).expect("rm");
    let r = green_runner(&g.th.paths.home);
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &local_opts(&g.th)).expect("doctor");
    assert!(!rep.ok());
    let c = check(&rep, "binary:rsync");
    assert!(!c.ok);
    assert!(c.detail.contains("not found"));
    assert!(check(&rep, "binary:ssh").ok);
}

#[test]
fn kache_remote_not_configured() {
    let g = green();
    let r = FakeRunner::new()
        .ok_args("kache", &["stats"], "Local: /x\nRemote:     (none)\n")
        .ok_args("kache", &["daemon", "status"], "not running\n")
        .ok_args("rustc", &["-vV"], VV_A)
        .ok_args("cargo", &["-V"], "cargo 1.95.0\n");
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &local_opts(&g.th)).expect("doctor");
    let c = check(&rep, "kache-remote");
    assert!(!c.ok);
    assert!(c.detail.contains("Remote:"));
    assert!(!check(&rep, "kache-daemon").ok);
    // rustup missing from the runner is fine: toolchain_name is informational
    assert!(check(&rep, "toolchain").ok);
}

#[test]
fn failed_command_is_a_failed_check_not_a_panic() {
    let g = green();
    let r = FakeRunner::new()
        .fail_args("kache", &["stats"], 2, "boom: no config")
        .fail("rustc", 1, "rustc exploded");
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &local_opts(&g.th)).expect("doctor");
    assert!(
        check(&rep, "kache-remote")
            .detail
            .contains("boom: no config")
    );
    let tc = check(&rep, "toolchain");
    assert!(!tc.ok);
    assert!(tc.detail.contains("rustc exploded"));
    // kache daemon status has no script -> spawn error -> failed check
    assert!(!check(&rep, "kache-daemon").ok);
}

#[test]
fn socket_missing_and_shim_not_first() {
    let mut g = green();
    g.cfg.local_server.socket = g.th.dir.path().join("nope.sock");
    // put the real-cargo dir before the shim dir
    g.th.paths.path_dirs.reverse();
    let r = green_runner(&g.th.paths.home);
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &local_opts(&g.th)).expect("doctor");
    let s = check(&rep, "server-socket");
    assert!(!s.ok);
    assert!(s.detail.contains("nope.sock"));
    let sh = check(&rep, "shim-precedence");
    assert!(!sh.ok);
    assert!(sh.detail.contains("export PATH="));
}

#[test]
fn remote_ssh_unreachable_fails_dependent_checks() {
    let g = green();
    let r = green_runner(&g.th.paths.home).fail_args(
        "ssh",
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=3",
            "node0",
            "true",
        ],
        255,
        "ssh: connect to host node0 port 22: No route to host",
    );
    let mut o = local_opts(&g.th);
    o.remote = true;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    let c = check(&rep, "remote-ssh");
    assert!(!c.ok);
    assert!(c.detail.contains("No route to host"));
    assert!(!rep.ok());
}

#[test]
fn remote_toolchain_mismatch_reports_both_fingerprints() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = green_runner(&cwd).ok_args("ssh", &["node0", &remote_cmd(&cwd)], VV_B);
    let mut o = local_opts(&g.th);
    o.remote = true;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    let c = check(&rep, "remote-toolchain");
    assert!(!c.ok);
    assert!(c.detail.contains("59807616e"), "{}", c.detail);
    assert!(c.detail.contains("abcdef123"), "{}", c.detail);
    assert!(c.detail.contains("1.96.0"), "{}", c.detail);
}

#[test]
fn remote_rbs_missing() {
    let g = green();
    let r = green_runner(&g.th.paths.home).fail_args(
        "ssh",
        &["node0", ".local/bin/rbs", "--version"],
        127,
        "bash: rbs: command not found",
    );
    let mut o = local_opts(&g.th);
    o.remote = true;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    let c = check(&rep, "remote-rbs");
    assert!(!c.ok);
    assert!(c.detail.contains("command not found"));
}

#[test]
fn remote_uses_configured_host() {
    let g = green();
    let mut cfg = g.cfg.clone();
    cfg.remote.host = "bigbox".into();
    let r = green_runner(&g.th.paths.home).ok("ssh", "");
    let mut o = local_opts(&g.th);
    o.remote = true;
    let _ = doctor_with(&r, &g.th.paths, &cfg, &o).expect("doctor");
    assert!(r.called_with("ssh", &["bigbox", ".local/bin/rbs", "--version"]));
}

#[test]
fn report_ok_is_all_checks() {
    let mut rep = DoctorReport::default();
    assert!(rep.ok());
    rep.checks.push(crate::Check {
        name: "a".into(),
        ok: true,
        detail: String::new(),
    });
    assert!(rep.ok());
    rep.checks.push(crate::Check {
        name: "b".into(),
        ok: false,
        detail: "x".into(),
    });
    assert!(!rep.ok());
}
