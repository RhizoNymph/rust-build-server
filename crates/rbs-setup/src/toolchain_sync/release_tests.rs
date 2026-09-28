use rbs_toolchain::ToolchainFingerprint;

use super::release::{Channel, Release, SyncPolicy, Winner, choose, commit_date, pin_channel};
use super::{Probe, SyncError};

const HOST: &str = "x86_64-unknown-linux-gnu";

fn probe(release: &str, commit: &str, date: Option<&str>, name: &str) -> Probe {
    Probe {
        fp: ToolchainFingerprint {
            rustc_commit: commit.into(),
            rustc_version: release.into(),
            host: HOST.into(),
            cargo_version: String::new(),
            toolchain_name: name.into(),
        },
        commit_date: date.map(str::to_string),
    }
}

#[test]
fn policy_parses_and_defaults_to_newest() {
    assert_eq!("newest".parse::<SyncPolicy>(), Ok(SyncPolicy::Newest));
    assert_eq!("oldest".parse::<SyncPolicy>(), Ok(SyncPolicy::Oldest));
    assert_eq!(SyncPolicy::default(), SyncPolicy::Newest);
    let err = "latest".parse::<SyncPolicy>().expect_err("unknown policy");
    assert!(err.contains("newest|oldest"), "{err}");
    assert_eq!(SyncPolicy::Oldest.to_string(), "oldest");
}

#[test]
fn parses_stable_beta_nightly() {
    let r = Release::parse("1.95.0", None).expect("stable");
    assert_eq!((r.major, r.minor, r.patch), (1, 95, 0));
    assert_eq!(r.channel, Channel::Stable);
    assert_eq!(
        Release::parse("1.96.0-beta.3", None).expect("beta").channel,
        Channel::Beta
    );
    assert_eq!(
        Release::parse("1.97.0-nightly", Some("2026-06-01"))
            .expect("nightly")
            .channel,
        Channel::Nightly
    );
}

#[test]
fn rejects_unknown_release_shapes() {
    for bad in ["", "1.95", "one.two.three", "1.95.0-dev", "1.95.0.1"] {
        assert!(
            matches!(
                Release::parse(bad, None),
                Err(SyncError::UnparsableRelease { .. })
            ),
            "{bad:?} must not parse"
        );
    }
}

#[test]
fn orders_by_version_then_channel_then_date() {
    let p = |s: &str, d: Option<&str>| Release::parse(s, d).expect("parse");
    assert!(p("1.95.0", None) < p("1.96.0", None));
    assert!(p("1.95.1", None) > p("1.95.0", None));
    assert!(
        p("1.100.0", None) > p("1.99.0", None),
        "numeric, not lexical"
    );
    assert!(p("1.97.0-nightly", None) < p("1.97.0-beta.1", None));
    assert!(p("1.97.0-beta.1", None) < p("1.97.0", None));
    assert!(p("1.97.0-nightly", Some("2026-06-01")) < p("1.97.0-nightly", Some("2026-06-02")));
    assert!(p("1.97.0-nightly", Some("2026-06-30")) < p("1.98.0-nightly", Some("2026-06-01")));
}

#[test]
fn extracts_commit_date() {
    let vv = "rustc 1.95.0\ncommit-hash: abc\ncommit-date: 2026-04-14\nhost: h\nrelease: 1.95.0\n";
    assert_eq!(commit_date(vv).as_deref(), Some("2026-04-14"));
    assert_eq!(commit_date("release: 1.95.0\n"), None);
    assert_eq!(commit_date("commit-date: unknown\n"), None);
}

#[test]
fn choose_newest_and_oldest() {
    let local = probe(
        "1.95.0",
        "aaa",
        Some("2026-04-14"),
        "1.95.0-x86_64-unknown-linux-gnu",
    );
    let remote = probe(
        "1.96.0",
        "bbb",
        Some("2026-06-01"),
        "stable-x86_64-unknown-linux-gnu",
    );
    let t = choose(SyncPolicy::Newest, &local, &remote).expect("newest");
    assert_eq!(t.winner, Winner::Remote);
    assert_eq!(
        t.release,
        Release::parse("1.96.0", Some("2026-06-01")).expect("parse")
    );
    let t = choose(SyncPolicy::Oldest, &local, &remote).expect("oldest");
    assert_eq!(t.winner, Winner::Local);
}

#[test]
fn choose_refuses_different_host_triples() {
    let local = probe("1.95.0", "aaa", None, "");
    let mut remote = probe("1.96.0", "bbb", None, "");
    remote.fp.host = "aarch64-unknown-linux-gnu".into();
    assert!(matches!(
        choose(SyncPolicy::Newest, &local, &remote),
        Err(SyncError::HostTripleMismatch { .. })
    ));
}

#[test]
fn choose_refuses_equal_order_with_different_commits() {
    let local = probe("1.95.0", "aaa", Some("2026-04-14"), "");
    let remote = probe("1.95.0", "bbb", Some("2026-04-14"), "");
    assert!(matches!(
        choose(SyncPolicy::Newest, &local, &remote),
        Err(SyncError::Undecidable { .. })
    ));
}

#[test]
fn stable_pins_exact_release_even_if_rustup_name_floats() {
    let r = Release::parse("1.96.0", None).expect("parse");
    assert_eq!(
        pin_channel(&r, "stable-x86_64-unknown-linux-gnu", HOST, "node0").expect("channel"),
        "1.96.0"
    );
    assert_eq!(
        pin_channel(&r, "", HOST, "node0").expect("no rustup"),
        "1.96.0"
    );
}

#[test]
fn prerelease_pins_dated_rustup_name() {
    let r = Release::parse("1.97.0-nightly", Some("2026-06-01")).expect("parse");
    assert_eq!(
        pin_channel(
            &r,
            "nightly-2026-06-02-x86_64-unknown-linux-gnu",
            HOST,
            "node0"
        )
        .expect("dated nightly"),
        "nightly-2026-06-02"
    );
    let b = Release::parse("1.96.0-beta.3", None).expect("parse");
    assert_eq!(
        pin_channel(&b, "beta-2026-05-20", HOST, "node0").expect("dated beta"),
        "beta-2026-05-20"
    );
}

#[test]
fn prerelease_with_floating_name_is_refused() {
    let r = Release::parse("1.97.0-nightly", Some("2026-06-01")).expect("parse");
    for name in [
        "nightly-x86_64-unknown-linux-gnu",
        "nightly",
        "",
        "my-custom-toolchain",
        "beta-2026-05-20-x86_64-unknown-linux-gnu",
    ] {
        let err = pin_channel(&r, name, HOST, "node0").expect_err(name);
        assert!(
            matches!(err, SyncError::FloatingChannel { .. }),
            "{name}: {err:?}"
        );
        assert!(err.to_string().contains("nightly-YYYY-MM-DD"), "{err}");
    }
}
