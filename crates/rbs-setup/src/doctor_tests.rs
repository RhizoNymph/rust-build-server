use std::path::Path;

use rbs_config::Config;

use crate::doctor::{REQUIRED_BINARIES, doctor_with};
use crate::testing::{FakeRunner, TempHome};
use crate::{DoctorOpts, DoctorReport, SyncPolicy};

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
        sync_toolchain: None,
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
    // A pinned directory: the hosts agreed on a toolchain and then diverged,
    // so this is a broken contract and must fail.
    std::fs::write(
        cwd.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    )
    .expect("write rust-toolchain.toml");
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
fn remote_toolchain_mismatch_in_unpinned_dir_is_not_a_failure() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    // No rust-toolchain.toml: the hosts' defaults may differ freely, and a
    // remote build from here falls back locally rather than breaking.
    let r = green_runner(&cwd).ok_args("ssh", &["node0", &remote_cmd(&cwd)], VV_B);
    let mut o = local_opts(&g.th);
    o.remote = true;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    let c = check(&rep, "remote-toolchain");
    assert!(c.ok, "unpinned mismatch must not fail: {}", c.detail);
    assert!(c.detail.contains("unpinned"), "{}", c.detail);
    assert!(c.detail.contains("rust-toolchain.toml"), "{}", c.detail);
    assert!(rep.ok(), "report must be green overall");
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

// ------------------------------------------------------------ --sync-toolchain

const LIST_A: &str = "1.95.0-x86_64-unknown-linux-gnu (default)\n";
const LIST_B: &str = "stable-x86_64-unknown-linux-gnu (default)\n";

fn login(cmd: &str) -> Vec<String> {
    crate::login_shell_args("node0", cmd)
}

fn strs(v: &[String]) -> Vec<&str> {
    v.iter().map(String::as_str).collect()
}

fn pin_of(cwd: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cwd.join("rust-toolchain.toml")).ok()?;
    crate::parse_toolchain_channel(&text)
}

/// Local on 1.95.0 (VV_A), node0 on 1.96.0 (VV_B), unpinned, every rustup
/// command scripted to succeed; `+1.95.0`/`+1.96.0` resolve to VV_A/VV_B on
/// both sides.
fn diverged_runner(cwd: &Path) -> FakeRunner {
    let pin = cwd.join("rust-toolchain.toml");
    green_runner(cwd)
        .ok_args("ssh", &["node0", &remote_cmd(cwd)], VV_B)
        .ok_args("rustup", &["toolchain", "list"], LIST_A)
        .ok_args(
            "rustup",
            &["toolchain", "install", "1.96.0", "--profile", "minimal"],
            "",
        )
        .ok_args(
            "rustup",
            &["toolchain", "install", "1.95.0", "--profile", "minimal"],
            "",
        )
        .ok_args("rustc", &["+1.95.0", "-vV"], VV_A)
        .ok_args("rustc", &["+1.96.0", "-vV"], VV_B)
        .ok_args("ssh", &strs(&login("rustup toolchain list")), LIST_B)
        .ok_args(
            "ssh",
            &strs(&login("rustup toolchain install 1.96.0 --profile minimal")),
            "",
        )
        .ok_args(
            "ssh",
            &strs(&login("rustup toolchain install 1.95.0 --profile minimal")),
            "",
        )
        .ok_args("ssh", &strs(&login("rustc +1.95.0 -vV")), VV_A)
        .ok_args("ssh", &strs(&login("rustc +1.96.0 -vV")), VV_B)
        .ok_args(
            "ssh",
            &[
                "node0",
                "test",
                "-d",
                &crate::shell_quote(&cwd.display().to_string()),
            ],
            "",
        )
        .ok_args(
            "scp",
            &[
                "-q",
                &pin.display().to_string(),
                &format!("node0:{}", pin.display()),
            ],
            "",
        )
}

fn sync_opts(th: &TempHome, policy: SyncPolicy) -> DoctorOpts {
    DoctorOpts {
        cwd: th.paths.home.clone(),
        remote: true,
        sync_toolchain: Some(policy),
    }
}

#[test]
fn sync_newest_installs_locally_and_pins() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = diverged_runner(&cwd);
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(s.ok, "{}", s.detail);
    assert!(s.detail.contains("1.96.0"), "{}", s.detail);
    assert!(s.detail.contains("newest"), "{}", s.detail);
    assert_eq!(pin_of(&cwd).as_deref(), Some("1.96.0"));
    assert!(r.called_with(
        "rustup",
        &["toolchain", "install", "1.96.0", "--profile", "minimal"]
    ));
    assert!(
        r.called_with(
            "ssh",
            &strs(&login("rustup toolchain install 1.96.0 --profile minimal"))
        ),
        "node0 has 1.96.0 only as `stable`, so the exact pin is installed there too"
    );
    let pin = cwd.join("rust-toolchain.toml");
    assert!(r.called_with(
        "scp",
        &[
            "-q",
            &pin.display().to_string(),
            &format!("node0:{}", pin.display())
        ]
    ));
    let rt = check(&rep, "remote-toolchain");
    assert!(rt.ok, "{}", rt.detail);
    assert!(rep.ok(), "{:?}", rep.checks);
}

#[test]
fn sync_oldest_installs_on_remote_only() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = diverged_runner(&cwd);
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Oldest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(s.ok, "{}", s.detail);
    assert!(s.detail.contains("oldest"), "{}", s.detail);
    assert_eq!(pin_of(&cwd).as_deref(), Some("1.95.0"));
    assert!(
        r.calls_to("rustup")
            .iter()
            .all(|a| a.get(1).map(String::as_str) != Some("install")),
        "1.95.0 is already installed locally: {:?}",
        r.calls_to("rustup")
    );
    assert!(r.called_with(
        "ssh",
        &strs(&login("rustup toolchain install 1.95.0 --profile minimal"))
    ));
    assert!(rep.ok(), "{:?}", rep.checks);
}

#[test]
fn sync_when_already_in_sync_changes_nothing() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = green_runner(&cwd);
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(s.ok, "{}", s.detail);
    assert!(s.detail.contains("already in sync"), "{}", s.detail);
    assert_eq!(pin_of(&cwd), None, "no pin written");
    assert!(r.calls_to("scp").is_empty());
    assert!(rep.ok(), "{:?}", rep.checks);
}

#[test]
fn sync_implies_remote() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = diverged_runner(&cwd);
    let mut o = sync_opts(&g.th, SyncPolicy::Newest);
    o.remote = false;
    let rep = doctor_with(&r, &g.th.paths, &g.cfg, &o).expect("doctor");
    assert!(check(&rep, "remote-ssh").ok);
    assert!(check(&rep, "toolchain-sync").ok);
}

#[test]
fn sync_install_failure_fails_and_leaves_pin_alone() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = diverged_runner(&cwd).fail_args(
        "ssh",
        &strs(&login("rustup toolchain install 1.95.0 --profile minimal")),
        1,
        "error: no disk space",
    );
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Oldest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(!s.ok);
    assert!(s.detail.contains("no disk space"), "{}", s.detail);
    assert!(s.detail.contains("node0"), "{}", s.detail);
    assert_eq!(
        pin_of(&cwd),
        None,
        "pin is only written once both hosts have the toolchain"
    );
    // The ordinary contract-aware check still reports the (unpinned) mismatch.
    assert!(check(&rep, "remote-toolchain").detail.contains("unpinned"));
    assert!(!rep.ok());
}

#[test]
fn sync_updates_an_existing_pin() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    std::fs::write(
        cwd.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\ncomponents = [\"clippy\"]\n",
    )
    .expect("write");
    let r = diverged_runner(&cwd);
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    assert!(check(&rep, "toolchain-sync").ok);
    let text = std::fs::read_to_string(cwd.join("rust-toolchain.toml")).expect("read");
    assert_eq!(
        text,
        "[toolchain]\nchannel = \"1.96.0\"\ncomponents = [\"clippy\"]\n"
    );
    assert!(rep.ok(), "{:?}", rep.checks);
}

#[test]
fn sync_skips_pin_push_when_remote_dir_is_not_mirrored_yet() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    let r = diverged_runner(&cwd).fail_args(
        "ssh",
        &[
            "node0",
            "test",
            "-d",
            &crate::shell_quote(&cwd.display().to_string()),
        ],
        1,
        "",
    );
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(s.ok, "{}", s.detail);
    assert!(s.detail.contains("next remote build"), "{}", s.detail);
    assert!(r.calls_to("scp").is_empty());
}

#[test]
fn sync_fails_when_verification_disagrees() {
    let g = green();
    let cwd = g.th.paths.home.clone();
    // node0's `+1.96.0` is some other build of 1.96.0.
    let other = VV_B.replace("abcdef1234567890", "0123456789abcdef");
    let r = diverged_runner(&cwd).ok_args("ssh", &strs(&login("rustc +1.96.0 -vV")), &other);
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(!s.ok);
    assert!(s.detail.contains("012345678"), "{}", s.detail);
    assert!(!rep.ok());
}

#[test]
fn sync_skipped_when_ssh_unreachable() {
    let g = green();
    let r = diverged_runner(&g.th.paths.home).fail_args(
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
        "no route",
    );
    let rep = doctor_with(
        &r,
        &g.th.paths,
        &g.cfg,
        &sync_opts(&g.th, SyncPolicy::Newest),
    )
    .expect("doctor");
    let s = check(&rep, "toolchain-sync");
    assert!(!s.ok);
    assert!(s.detail.contains("skipped"), "{}", s.detail);
}
