use std::path::Path;

use rbs_config::Config;

use crate::setup::{
    KACHE_CONFIG_DEFAULT, STORE_GC_TIMER, SetupError, aws_credentials_with_profile,
    generate_config, kache_config, line_diff, render_store_gc_service, render_unit, setup_with,
};
use crate::testing::{FakeRunner, TempHome, mode_of, read};
use crate::{Role, SetupOpts};

const KACHE_STATS_OK: &str = "Local:      /home/u/.cache/kache (1.2 GiB)\nRemote:     s3://kache/artifacts (profile rbs)\nHits:       0\n";

fn opts(role: Role, th: &TempHome) -> SetupOpts {
    SetupOpts {
        role,
        remote_host: None,
        force: false,
        self_exe: th.paths.self_exe.clone(),
    }
}

fn runner() -> FakeRunner {
    FakeRunner::new()
        .ok("kache", KACHE_STATS_OK)
        .ok("systemctl", "")
        .ok("scp", "")
        .ok("ssh", "")
}

fn parsed(text: &str) -> Config {
    Config::from_toml(Path::new("generated.toml"), text).expect("generated config must parse")
}

#[test]
fn generated_laptop_config_round_trips_with_laptop_sizing() {
    let cfg = parsed(&generate_config(Role::Laptop));
    assert_eq!(cfg.server.reserve_cores, 4);
    assert_eq!(cfg.server.max_jobs, 3);
    assert_eq!(cfg.server.job_mem_max_gib, 12);
    assert_eq!(cfg.server.min_mem_available_gib, 8);
    assert!(cfg.remote.enabled);
    assert_eq!(cfg.remote.host, "node0");
    assert_eq!(cfg.policy, rbs_config::Policy::default());
}

#[test]
fn generated_node0_config_disables_remote_and_uses_big_sizing() {
    let cfg = parsed(&generate_config(Role::Node0));
    assert_eq!(cfg.server.reserve_cores, 4);
    assert_eq!(cfg.server.max_jobs, 12);
    assert_eq!(cfg.server.job_mem_max_gib, 24);
    assert_eq!(cfg.server.min_mem_available_gib, 16);
    assert!(!cfg.remote.enabled);
}

#[test]
fn setup_writes_config_when_absent() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    let text = read(&th.paths.rbs_config());
    assert_eq!(text, generate_config(Role::Laptop));
}

#[test]
fn existing_config_is_kept_without_force() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let p = th.paths.rbs_config();
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(&p, "[remote]\nhost = \"other\"\n").expect("write");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup continues");
    assert_eq!(read(&p), "[remote]\nhost = \"other\"\n");
}

#[test]
fn force_overwrites_existing_config() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let p = th.paths.rbs_config();
    std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
    std::fs::write(&p, "[remote]\nhost = \"other\"\n").expect("write");
    let r = runner();
    let mut o = opts(Role::Node0, &th);
    o.force = true;
    setup_with(&r, &th.paths, &o).expect("setup");
    assert_eq!(read(&p), generate_config(Role::Node0));
}

#[test]
fn line_diff_marks_changes() {
    let d = line_diff("a\nb\nc\n", "a\nx\nc\nd\n");
    assert_eq!(d, " a\n-b\n+x\n c\n+d\n");
    assert_eq!(line_diff("same\n", "same\n"), " same\n");
}

#[test]
fn kache_config_is_exact_text() {
    assert_eq!(
        KACHE_CONFIG_DEFAULT,
        "[cache.remote]\ntype = \"s3\"\nbucket = \"kache\"\nendpoint = \"http://127.0.0.1:9100\"\nregion = \"us-east-1\"\nprofile = \"rbs\"\nprefix = \"artifacts\"\n"
    );
    assert_eq!(
        kache_config("http://127.0.0.1:9100", "kache"),
        KACHE_CONFIG_DEFAULT
    );
    assert!(kache_config("http://x:1", "b").contains("endpoint = \"http://x:1\"\nregion"));
    assert!(kache_config("http://x:1", "b").contains("bucket = \"b\"\n"));
}

#[test]
fn setup_writes_kache_config_and_validates_with_kache_stats() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    assert_eq!(read(&th.paths.kache_config()), KACHE_CONFIG_DEFAULT);
    assert!(r.called_with("kache", &["stats"]));
}

#[test]
fn kache_stats_without_remote_is_an_error() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = FakeRunner::new()
        .ok_args("kache", &["stats"], "Local: /x\nRemote:     (none)\n")
        .ok("kache", "")
        .ok("systemctl", "");
    let err = setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect_err("must fail");
    assert!(matches!(err, SetupError::KacheRemoteNotConfigured { .. }));
    assert!(!err.to_string().contains("SK"), "secrets never printed");
}

#[test]
fn missing_minio_env_is_a_structured_error() {
    let th = TempHome::new();
    let r = runner();
    let err = setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect_err("must fail");
    assert!(matches!(err, SetupError::MissingMinioEnv { .. }));
    assert!(err.to_string().contains("minio.env"));
}

#[test]
fn aws_credentials_append_preserves_other_profiles() {
    let existing = "[default]\naws_access_key_id = D1\naws_secret_access_key = D2\n";
    let out = aws_credentials_with_profile(existing, "rbs", "AK", "SK");
    assert!(out.starts_with(existing));
    assert!(out.contains("\n[rbs]\naws_access_key_id = AK\naws_secret_access_key = SK\n"));
    // idempotent
    assert_eq!(aws_credentials_with_profile(&out, "rbs", "AK", "SK"), out);
}

#[test]
fn aws_credentials_replace_only_rbs_section() {
    let existing = "[rbs]\naws_access_key_id = OLD\naws_secret_access_key = OLD2\n[other]\naws_access_key_id = O\n";
    let out = aws_credentials_with_profile(existing, "rbs", "AK", "SK");
    assert!(out.contains("[rbs]\naws_access_key_id = AK\naws_secret_access_key = SK\n"));
    assert!(out.contains("[other]\naws_access_key_id = O\n"));
    assert!(!out.contains("OLD"));
}

#[test]
fn setup_writes_aws_credentials_with_mode_600() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let creds = th.paths.aws_credentials();
    std::fs::create_dir_all(creds.parent().expect("parent")).expect("mkdir");
    std::fs::write(&creds, "[default]\naws_access_key_id = D1\n").expect("write");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    let text = read(&creds);
    assert!(text.starts_with("[default]\naws_access_key_id = D1\n"));
    assert!(text.contains("[rbs]\naws_access_key_id = AK\naws_secret_access_key = SK\n"));
    assert_eq!(mode_of(&creds), 0o600);
}

#[test]
fn systemd_unit_content_and_commands() {
    let unit = render_unit(Path::new("/home/u/.local/bin/rbs"));
    assert!(unit.contains("ExecStart=/home/u/.local/bin/rbs server\n"));
    assert!(unit.contains("Restart=on-failure\n"));
    assert!(unit.contains("Environment=RBS_LOG=info\n"));
    assert!(unit.contains("WantedBy=default.target\n"));
    assert!(!unit.contains("{self_exe}"));

    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    let written = read(&th.paths.systemd_user_dir().join("rbs-server.service"));
    assert_eq!(written, render_unit(&th.paths.self_exe));
    assert!(r.called_with("systemctl", &["--user", "daemon-reload"]));
    assert!(r.called_with("systemctl", &["--user", "enable", "--now", "rbs-server"]));
    assert!(r.called_with("kache", &["daemon", "install"]));
}

#[test]
fn node0_installs_store_gc_timer_units() {
    let unit = render_store_gc_service(Path::new("/home/u/.local/bin/rbs"));
    assert!(unit.contains("Type=oneshot\n"));
    assert!(unit.contains("ExecStart=/home/u/.local/bin/rbs store-gc\n"));
    assert!(unit.contains("/snap/bin"));
    assert!(!unit.contains("{self_exe}"));
    assert!(STORE_GC_TIMER.contains("OnCalendar=daily\n"));
    assert!(STORE_GC_TIMER.contains("RandomizedDelaySec=1h\n"));
    assert!(STORE_GC_TIMER.contains("Persistent=true\n"));
    assert!(STORE_GC_TIMER.contains("WantedBy=timers.target\n"));

    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Node0, &th)).expect("setup");
    let service = read(&th.paths.systemd_user_dir().join("rbs-store-gc.service"));
    assert_eq!(service, render_store_gc_service(&th.paths.self_exe));
    let timer = read(&th.paths.systemd_user_dir().join("rbs-store-gc.timer"));
    assert_eq!(timer, STORE_GC_TIMER);
    assert!(r.called_with("systemctl", &["--user", "daemon-reload"]));
    assert!(r.called_with(
        "systemctl",
        &["--user", "enable", "--now", "rbs-store-gc.timer"]
    ));
}

#[test]
fn laptop_does_not_install_store_gc_units() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    assert!(
        !th.paths
            .systemd_user_dir()
            .join("rbs-store-gc.service")
            .exists()
    );
    assert!(
        !th.paths
            .systemd_user_dir()
            .join("rbs-store-gc.timer")
            .exists()
    );
    assert!(!r.called_with(
        "systemctl",
        &["--user", "enable", "--now", "rbs-store-gc.timer"]
    ));
}

#[test]
fn kache_daemon_already_installed_is_ignored() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = FakeRunner::new()
        .ok_args("kache", &["stats"], KACHE_STATS_OK)
        .fail_args(
            "kache",
            &["daemon", "install"],
            1,
            "error: daemon already installed",
        )
        .ok("systemctl", "");
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("already installed is fine");
}

#[test]
fn shim_is_linked_to_self_exe() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("setup");
    let shim = th.paths.shim_dir().join("cargo");
    assert!(shim.exists());
    assert_eq!(read(&shim), read(&th.paths.self_exe));
    // re-running replaces the link instead of failing
    setup_with(&r, &th.paths, &opts(Role::Laptop, &th)).expect("idempotent");
}

#[test]
fn remote_host_triggers_scp_and_ssh_setup() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    let mut o = opts(Role::Laptop, &th);
    o.remote_host = Some("node0".into());
    o.force = true;
    setup_with(&r, &th.paths, &o).expect("setup");
    let exe = th.paths.self_exe.display().to_string();
    assert!(r.called_with("ssh", &["node0", "mkdir", "-p", ".local/bin"]));
    assert!(r.called_with("scp", &[&exe, "node0:.local/bin/rbs"]));
    assert!(r.called_with(
        "ssh",
        &[
            "node0",
            ".local/bin/rbs",
            "setup",
            "--role",
            "node0",
            "--force"
        ]
    ));
}

#[test]
fn node0_role_never_recurses_to_remote() {
    let th = TempHome::new();
    th.write_minio_env("AK", "SK");
    let r = runner();
    let mut o = opts(Role::Node0, &th);
    o.remote_host = Some("node0".into());
    setup_with(&r, &th.paths, &o).expect("setup");
    assert!(r.calls_to("scp").is_empty());
    assert!(r.calls_to("ssh").is_empty());
}
