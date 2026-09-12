use std::path::Path;

use rbs_config::Config;

use crate::bootstrap::{
    BootstrapError, BootstrapOpts, StepStatus, bootstrap_with, doctor_failed_checks,
    find_toolchain_file, parse_toolchain_channel, plan_hosts, render_report, ssh_config_hostname,
    toolchain_installed,
};
use crate::runner::Output;
use crate::testing::{FakeRunner, TempHome};
use crate::{BootstrapReport, Role};

const SERVER: &str = "node0";
const CLIENT: &str = "lap";
const CHANNEL: &str = "1.95.0";

fn alias_probe(server: &str) -> String {
    format!("test -f .ssh/config && grep -q 'Host {server}' .ssh/config")
}

fn config(server: &str, clients: &[&str]) -> Config {
    let mut c = Config::default();
    c.bootstrap.server = server.to_string();
    c.bootstrap.clients = clients.iter().map(|s| s.to_string()).collect();
    c.bootstrap.toolchain = CHANNEL.to_string();
    c
}

fn opts(th: &TempHome) -> BootstrapOpts {
    BootstrapOpts {
        server: None,
        clients: Vec::new(),
        dry_run: false,
        force: false,
        self_exe: th.paths.self_exe.clone(),
        workspace: th.dir.path().join("ws"),
    }
}

/// A home with a local kache binary to push, and an ssh config naming the server.
fn home() -> TempHome {
    let th = TempHome::new();
    th.fake_bin(&th.paths.home.join(".local/bin"), "kache");
    let ssh_cfg = th.paths.home.join(".ssh/config");
    std::fs::create_dir_all(ssh_cfg.parent().expect("parent")).expect("mkdir");
    std::fs::write(&ssh_cfg, "Host node0\n  HostName 10.0.0.5\n  User u\n").expect("write");
    th
}

/// Every ssh/scp succeeds; the wanted channel is already installed everywhere.
fn runner(hosts: &[&str]) -> FakeRunner {
    let mut r = FakeRunner::new().ok("ssh", "").ok("scp", "");
    for h in hosts {
        r = r.ok_args(
            "ssh",
            &[h, "rustup toolchain list"],
            "1.95.0-x86_64-unknown-linux-gnu (default)\n",
        );
    }
    r
}

fn host<'a>(report: &'a BootstrapReport, name: &str) -> &'a crate::bootstrap::HostReport {
    report
        .hosts
        .iter()
        .find(|h| h.host == name)
        .unwrap_or_else(|| panic!("no report for {name}"))
}

fn status(report: &BootstrapReport, name: &str, step: &str) -> StepStatus {
    host(report, name)
        .status(step)
        .unwrap_or_else(|| panic!("{name} has no step {step}"))
}

// ---------------------------------------------------------------- pure helpers

#[test]
fn ssh_config_parser_reads_hostname_blocks() {
    let text = "\
# a comment
Host alpha
    HostName 10.0.0.1
    User u

Host beta gamma
    Hostname 10.0.0.2

Host nohost
    User only
";
    assert_eq!(
        ssh_config_hostname(text, "alpha").as_deref(),
        Some("10.0.0.1")
    );
    // keywords are case-insensitive and a Host line may list several aliases
    assert_eq!(
        ssh_config_hostname(text, "beta").as_deref(),
        Some("10.0.0.2")
    );
    assert_eq!(
        ssh_config_hostname(text, "gamma").as_deref(),
        Some("10.0.0.2")
    );
    assert_eq!(ssh_config_hostname(text, "nohost"), None);
    assert_eq!(ssh_config_hostname(text, "missing"), None);
    assert_eq!(ssh_config_hostname("", "alpha"), None);
}

#[test]
fn ssh_config_parser_ignores_comments_and_wildcards() {
    let text = "\
Host *
    HostName should.not.match
#Host alpha
#    HostName commented.out
Host alpha
    HostName 10.0.0.9
";
    assert_eq!(
        ssh_config_hostname(text, "alpha").as_deref(),
        Some("10.0.0.9")
    );
    assert_eq!(ssh_config_hostname(text, "other"), None);
}

#[test]
fn toolchain_channel_is_parsed_from_rust_toolchain_toml() {
    assert_eq!(
        parse_toolchain_channel("[toolchain]\nchannel = \"1.95.0\"\ncomponents = [\"clippy\"]\n")
            .as_deref(),
        Some("1.95.0")
    );
    assert_eq!(
        parse_toolchain_channel("[toolchain]\ncomponents = [\"clippy\"]\n"),
        None
    );
    assert_eq!(parse_toolchain_channel("not toml ["), None);

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.95.0\"\n",
    )
    .expect("write");
    assert_eq!(
        find_toolchain_file(&root.join("a/b")),
        Some(root.join("rust-toolchain.toml"))
    );
    assert_eq!(find_toolchain_file(Path::new("/nonexistent/x")), None);
}

#[test]
fn installed_toolchains_are_matched_by_channel_prefix() {
    let list = "1.90.0-x86_64-unknown-linux-gnu\n1.95.0-x86_64-unknown-linux-gnu (default)\n";
    assert!(toolchain_installed(list, "1.95.0"));
    assert!(toolchain_installed(list, "1.90.0"));
    assert!(!toolchain_installed(list, "1.9"));
    assert!(!toolchain_installed(list, "nightly"));
    assert!(!toolchain_installed("", "1.95.0"));
}

#[test]
fn doctor_failures_are_extracted_by_name() {
    let out = "ok   binary:rbs    /home/u/.local/bin/rbs\nFAIL kache-daemon  not running\nFAIL server-socket missing\n";
    assert_eq!(
        doctor_failed_checks(out),
        vec!["kache-daemon".to_string(), "server-socket".to_string()]
    );
    assert!(doctor_failed_checks("ok   binary:rbs  x\n").is_empty());
}

#[test]
fn plan_hosts_puts_the_server_first_and_dedupes() {
    assert_eq!(
        plan_hosts("node0", &["a".into(), "b".into()]),
        vec![
            ("node0".to_string(), Role::Server),
            ("a".to_string(), Role::Client),
            ("b".to_string(), Role::Client),
        ]
    );
    // a host listed as both keeps the server role, once
    assert_eq!(
        plan_hosts("node0", &["node0".into(), "a".into(), "a".into()]),
        vec![
            ("node0".to_string(), Role::Server),
            ("a".to_string(), Role::Client),
        ]
    );
    assert_eq!(
        plan_hosts("", &["a".into()]),
        vec![("a".to_string(), Role::Client)]
    );
    assert!(plan_hosts("", &[]).is_empty());
}

#[test]
fn no_hosts_configured_is_a_structured_error() {
    let th = home();
    let r = FakeRunner::new();
    let mut c = config("", &[]);
    c.bootstrap.server = String::new();
    let err = bootstrap_with(&r, &th.paths, &c, &opts(&th)).expect_err("must fail");
    assert!(matches!(err, BootstrapError::NoHosts));
    assert!(r.calls.borrow().is_empty());
}

// ------------------------------------------------------------------ per-step

#[test]
fn binaries_are_pushed_atomically_via_a_new_file_and_mv() {
    let th = home();
    let r = runner(&[SERVER]);
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    let exe = th.paths.self_exe.display().to_string();
    let kache = th.paths.home.join(".local/bin/kache").display().to_string();

    assert!(r.called_with("ssh", &[SERVER, "mkdir", "-p", ".local/bin"]));
    assert!(r.called_with("scp", &[&exe, "node0:.local/bin/rbs.new"]));
    assert!(r.called_with(
        "ssh",
        &[SERVER, "mv", "-f", ".local/bin/rbs.new", ".local/bin/rbs"]
    ));
    assert!(r.called_with("scp", &[&kache, "node0:.local/bin/kache.new"]));
    assert!(r.called_with(
        "ssh",
        &[
            SERVER,
            "mv",
            "-f",
            ".local/bin/kache.new",
            ".local/bin/kache"
        ]
    ));
    // never overwritten in place: a running server holds its own binary open
    assert!(!r.called_with("scp", &[&exe, "node0:.local/bin/rbs"]));
    assert_eq!(status(&report, SERVER, "binaries"), StepStatus::Ok);
}

#[test]
fn missing_local_kache_is_a_note_not_a_failure() {
    let th = TempHome::new(); // no ~/.local/bin/kache
    let r = runner(&[SERVER]);
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "binaries"), StepStatus::Ok);
    assert!(report.ok(), "a missing local kache must not fail the host");
    assert!(
        host(&report, SERVER)
            .notes
            .iter()
            .any(|n| n.contains("kache")),
        "expected a kache note, got {:?}",
        host(&report, SERVER).notes
    );
    assert!(
        !r.calls_to("scp")
            .iter()
            .any(|a| a.iter().any(|s| s.contains("kache")))
    );
}

#[test]
fn secrets_are_not_pushed_by_default_and_the_note_names_the_scp_command() {
    let th = home();
    let r = runner(&[SERVER]).fail_args("ssh", &[SERVER, "test -f .config/rbs/minio.env"], 1, "");
    th.write_minio_env("AK", "SK");
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");

    assert_eq!(status(&report, SERVER, "secrets"), StepStatus::Skipped);
    let notes = host(&report, SERVER).notes.join("\n");
    assert!(
        notes.contains(&format!(
            "scp {} node0:.config/rbs/minio.env",
            th.paths.minio_env().display()
        )),
        "actionable note missing: {notes}"
    );
    assert!(!notes.contains("AK") && !notes.contains("SK"), "no secrets");
    assert!(
        !r.calls_to("scp")
            .iter()
            .any(|a| a.iter().any(|s| s.contains("minio.env")))
    );
    assert!(!r.called_with("ssh", &[SERVER, "mkdir", "-p", ".config/rbs"]));
}

#[test]
fn secrets_present_on_the_host_need_no_note() {
    let th = home();
    let r = runner(&[SERVER]); // `test -f` succeeds via the ssh wildcard
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "secrets"), StepStatus::Skipped);
    assert!(
        !host(&report, SERVER)
            .notes
            .iter()
            .any(|n| n.contains("minio.env"))
    );
}

#[test]
fn push_secrets_copies_minio_env_with_mode_600() {
    let th = home();
    th.write_minio_env("AK", "SK");
    let r = runner(&[SERVER]);
    let mut c = config(SERVER, &[]);
    c.bootstrap.push_secrets = true;
    let report = bootstrap_with(&r, &th.paths, &c, &opts(&th)).expect("bootstrap");

    let env = th.paths.minio_env().display().to_string();
    assert!(r.called_with("ssh", &[SERVER, "mkdir", "-p", ".config/rbs"]));
    assert!(r.called_with("scp", &[&env, "node0:.config/rbs/minio.env"]));
    assert!(r.called_with("ssh", &[SERVER, "chmod", "600", ".config/rbs/minio.env"]));
    assert_eq!(status(&report, SERVER, "secrets"), StepStatus::Ok);
    let rendered = render_report(&report);
    assert!(!rendered.contains("AK") && !rendered.contains("SK"));
}

#[test]
fn push_secrets_without_a_local_file_is_a_note() {
    let th = home(); // no minio.env written
    let r = runner(&[SERVER]);
    let mut c = config(SERVER, &[]);
    c.bootstrap.push_secrets = true;
    let report = bootstrap_with(&r, &th.paths, &c, &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "secrets"), StepStatus::Skipped);
    assert!(
        host(&report, SERVER)
            .notes
            .iter()
            .any(|n| n.contains("minio.env"))
    );
    assert!(
        !r.calls_to("scp")
            .iter()
            .any(|a| a.iter().any(|s| s.contains("minio.env")))
    );
}

#[test]
fn ssh_alias_is_appended_only_when_the_client_lacks_it() {
    let th = home();
    let probe = alias_probe(SERVER);
    let r = runner(&[SERVER, CLIENT]).fail_args("ssh", &[CLIENT, &probe], 1, "");
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");

    assert!(r.called_with("ssh", &[CLIENT, &probe]));
    assert!(r.called_with(
        "ssh",
        &[
            CLIENT,
            "mkdir -p .ssh && chmod 700 .ssh && printf '%s\\n' 'Host node0' '  HostName 10.0.0.5' '  StrictHostKeyChecking accept-new' >> .ssh/config"
        ]
    ));
    assert_eq!(status(&report, CLIENT, "ssh-alias"), StepStatus::Ok);
    // the server never gets an alias to itself
    assert_eq!(status(&report, SERVER, "ssh-alias"), StepStatus::Skipped);
}

#[test]
fn existing_ssh_alias_is_left_alone() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]); // probe succeeds
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, CLIENT, "ssh-alias"), StepStatus::Skipped);
    assert!(
        !r.calls_to("ssh")
            .iter()
            .any(|a| a.iter().any(|s| s.contains(">> .ssh/config")))
    );
}

#[test]
fn missing_local_hostname_for_the_server_is_a_note() {
    let th = TempHome::new(); // no ~/.ssh/config
    th.fake_bin(&th.paths.home.join(".local/bin"), "kache");
    let probe = alias_probe(SERVER);
    let r = runner(&[SERVER, CLIENT]).fail_args("ssh", &[CLIENT, &probe], 1, "");
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, CLIENT, "ssh-alias"), StepStatus::Skipped);
    assert!(
        host(&report, CLIENT)
            .notes
            .iter()
            .any(|n| n.contains("HostName")),
        "{:?}",
        host(&report, CLIENT).notes
    );
    assert!(
        !r.calls_to("ssh")
            .iter()
            .any(|a| a.iter().any(|s| s.contains(">> .ssh/config")))
    );
}

#[test]
fn toolchain_is_installed_only_when_absent() {
    let th = home();
    let r = runner(&[SERVER]); // list already contains 1.95.0
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "toolchain"), StepStatus::Skipped);
    assert!(!r.called_with(
        "ssh",
        &[SERVER, "rustup toolchain install 1.95.0 --profile minimal"]
    ));

    let r = FakeRunner::new().ok("ssh", "").ok("scp", "").ok_args(
        "ssh",
        &[SERVER, "rustup toolchain list"],
        "1.90.0-x86_64-unknown-linux-gnu (default)\n",
    );
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert!(r.called_with(
        "ssh",
        &[SERVER, "rustup toolchain install 1.95.0 --profile minimal"]
    ));
    assert_eq!(status(&report, SERVER, "toolchain"), StepStatus::Ok);
}

#[test]
fn missing_rustup_on_the_host_is_an_actionable_note() {
    let th = home();
    let r = runner(&[SERVER]).fail_args(
        "ssh",
        &[SERVER, "rustup toolchain list"],
        127,
        "bash: rustup: command not found",
    );
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "toolchain"), StepStatus::Skipped);
    assert!(
        host(&report, SERVER)
            .notes
            .iter()
            .any(|n| n.contains("sh.rustup.rs")),
        "{:?}",
        host(&report, SERVER).notes
    );
    assert!(!r.called_with(
        "ssh",
        &[SERVER, "rustup toolchain install 1.95.0 --profile minimal"]
    ));
}

#[test]
fn toolchain_channel_comes_from_the_workspace_when_config_is_empty() {
    let th = home();
    let ws = th.dir.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir");
    std::fs::write(
        ws.join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.42.0\"\n",
    )
    .expect("write");
    let mut c = config(SERVER, &[]);
    c.bootstrap.toolchain = String::new();
    let r = FakeRunner::new().ok("ssh", "").ok("scp", "");
    bootstrap_with(&r, &th.paths, &c, &opts(&th)).expect("bootstrap");
    assert!(r.called_with(
        "ssh",
        &[SERVER, "rustup toolchain install 1.42.0 --profile minimal"]
    ));
}

#[test]
fn unknown_toolchain_channel_is_a_note_not_a_failure() {
    let th = home();
    let mut c = config(SERVER, &[]);
    c.bootstrap.toolchain = String::new(); // and no rust-toolchain.toml under the workspace
    let r = runner(&[SERVER]);
    let report = bootstrap_with(&r, &th.paths, &c, &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "toolchain"), StepStatus::Skipped);
    assert!(report.ok());
    assert!(!r.called_with("ssh", &[SERVER, "rustup toolchain list"]));
}

#[test]
fn setup_runs_remotely_with_the_host_role_and_force() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]);
    let mut o = opts(&th);
    o.force = true;
    let report = bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &o).expect("bootstrap");
    assert!(r.called_with(
        "ssh",
        &[
            SERVER,
            ".local/bin/rbs",
            "setup",
            "--role",
            "server",
            "--force"
        ]
    ));
    assert!(r.called_with(
        "ssh",
        &[
            CLIENT,
            ".local/bin/rbs",
            "setup",
            "--role",
            "client",
            "--force"
        ]
    ));
    assert_eq!(status(&report, SERVER, "setup"), StepStatus::Ok);

    let r = runner(&[SERVER]);
    bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert!(r.called_with(
        "ssh",
        &[SERVER, ".local/bin/rbs", "setup", "--role", "server"]
    ));
}

#[test]
fn doctor_uses_remote_only_for_clients() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]);
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");
    assert!(r.called_with("ssh", &[SERVER, ".local/bin/rbs", "doctor"]));
    assert!(r.called_with("ssh", &[CLIENT, ".local/bin/rbs", "doctor", "--remote"]));
    assert!(!r.called_with("ssh", &[SERVER, ".local/bin/rbs", "doctor", "--remote"]));
    assert_eq!(status(&report, CLIENT, "doctor"), StepStatus::Ok);
    assert!(report.ok());
}

#[test]
fn failing_doctor_marks_the_host_failed_and_names_the_checks() {
    let th = home();
    let r = runner(&[SERVER]).fail_args("ssh", &[SERVER, ".local/bin/rbs", "doctor"], 1, "");
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert_eq!(status(&report, SERVER, "doctor"), StepStatus::Failed);
    assert!(!report.ok());
    assert!(!host(&report, SERVER).ok());
}

#[test]
fn doctor_failure_detail_lists_check_names() {
    let th = home();
    let r = runner(&[SERVER]).script(
        "ssh",
        &[SERVER, ".local/bin/rbs", "doctor"],
        Output {
            status: Some(1),
            stdout: "ok   binary:rbs    /home/u/.local/bin/rbs\nFAIL kache-daemon  not running\nFAIL server-socket missing\n".into(),
            stderr: String::new(),
        },
    );
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    let step = host(&report, SERVER)
        .steps
        .iter()
        .find(|s| s.name == "doctor")
        .expect("doctor step");
    assert_eq!(step.status, StepStatus::Failed);
    assert!(step.detail.contains("kache-daemon"), "{}", step.detail);
    assert!(step.detail.contains("server-socket"), "{}", step.detail);
}

// ------------------------------------------------------------------ fleet

#[test]
fn an_unreachable_client_does_not_abort_the_others() {
    let th = home();
    let dead = "dead";
    let r = runner(&[SERVER, CLIENT, dead]).fail_args(
        "ssh",
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            dead,
            "true",
        ],
        255,
        "ssh: connect to host dead port 22: No route to host",
    );
    let report = bootstrap_with(&r, &th.paths, &config(SERVER, &[dead, CLIENT]), &opts(&th))
        .expect("bootstrap");

    assert_eq!(report.hosts.len(), 3);
    assert_eq!(status(&report, dead, "reach"), StepStatus::Failed);
    assert!(!host(&report, dead).ok());
    // nothing else was attempted on the dead host
    assert!(host(&report, dead).status("binaries").is_none());
    assert!(!r.called_with("ssh", &[dead, "mkdir", "-p", ".local/bin"]));
    // the healthy hosts were still provisioned
    assert!(host(&report, SERVER).ok());
    assert!(host(&report, CLIENT).ok());
    assert!(r.called_with(
        "ssh",
        &[CLIENT, ".local/bin/rbs", "setup", "--role", "client"]
    ));
    assert!(!report.ok(), "one failed host fails the run");
}

#[test]
fn reachability_is_probed_with_batchmode_and_a_timeout() {
    let th = home();
    let r = runner(&[SERVER]);
    bootstrap_with(&r, &th.paths, &config(SERVER, &[]), &opts(&th)).expect("bootstrap");
    assert!(r.called_with(
        "ssh",
        &[
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            SERVER,
            "true"
        ]
    ));
}

#[test]
fn report_ok_is_the_and_of_every_host() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]);
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");
    assert!(report.ok());
    assert!(report.hosts.iter().all(|h| h.ok()));

    let rendered = render_report(&report);
    assert!(rendered.contains(SERVER) && rendered.contains(CLIENT));
    assert!(rendered.contains("server") && rendered.contains("client"));
    assert!(
        rendered.contains("2 hosts"),
        "summary line missing:\n{rendered}"
    );
    assert!(BootstrapReport::default().ok(), "an empty report is ok");
}

#[test]
fn cli_flags_override_the_config() {
    let th = home();
    let r = runner(&["srv", "c1", "c2"]);
    let mut o = opts(&th);
    o.server = Some("srv".into());
    o.clients = vec!["c1".into(), "c2".into()];
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &["ignored"]), &o).expect("bootstrap");
    let hosts: Vec<&str> = report.hosts.iter().map(|h| h.host.as_str()).collect();
    assert_eq!(hosts, vec!["srv", "c1", "c2"]);
    assert_eq!(host(&report, "srv").role, Role::Server);
    assert_eq!(host(&report, "c1").role, Role::Client);
}

#[test]
fn dry_run_executes_nothing() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]);
    let mut o = opts(&th);
    o.dry_run = true;
    let report = bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &o).expect("bootstrap");
    assert!(r.calls.borrow().is_empty(), "dry run must not run anything");
    assert_eq!(report.hosts.len(), 2);
    assert!(report.ok());
    assert!(
        report
            .hosts
            .iter()
            .all(|h| h.steps.iter().all(|s| s.status == StepStatus::Skipped))
    );
}

#[test]
fn rerunning_a_healthy_fleet_only_repushes_binaries() {
    let th = home();
    let r = runner(&[SERVER, CLIENT]);
    let report =
        bootstrap_with(&r, &th.paths, &config(SERVER, &[CLIENT]), &opts(&th)).expect("bootstrap");
    assert!(report.ok());
    for h in &report.hosts {
        assert_eq!(h.status("binaries"), Some(StepStatus::Ok));
        assert_eq!(h.status("secrets"), Some(StepStatus::Skipped));
        assert_eq!(h.status("toolchain"), Some(StepStatus::Skipped));
        assert_eq!(h.status("ssh-alias"), Some(StepStatus::Skipped));
    }
    assert!(
        !r.calls_to("ssh")
            .iter()
            .any(|a| a.iter().any(|s| s.contains("rustup toolchain install")))
    );
}
