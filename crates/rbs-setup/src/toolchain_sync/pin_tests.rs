use super::SyncError;
use super::pin::{set_channel, write_pin};
use crate::parse_toolchain_channel;

#[test]
fn replaces_channel_and_keeps_everything_else() {
    let text = "# pinned for the fleet\n[toolchain]\nchannel = \"1.95.0\" # bump together\ncomponents = [\"clippy\", \"rustfmt\"]\nprofile = \"minimal\"\n";
    let out = set_channel(text, "1.96.0").expect("edit");
    assert_eq!(
        out,
        "# pinned for the fleet\n[toolchain]\nchannel = \"1.96.0\"\ncomponents = [\"clippy\", \"rustfmt\"]\nprofile = \"minimal\"\n"
    );
}

#[test]
fn only_touches_the_toolchain_table() {
    let text = "[other]\nchannel = \"keep\"\n\n[toolchain]\ncomponents = []\n";
    let out = set_channel(text, "1.96.0").expect("edit");
    assert_eq!(
        out,
        "[other]\nchannel = \"keep\"\n\n[toolchain]\nchannel = \"1.96.0\"\ncomponents = []\n"
    );
    assert_eq!(parse_toolchain_channel(&out).as_deref(), Some("1.96.0"));
}

#[test]
fn appends_table_when_missing() {
    let out = set_channel("", "1.96.0").expect("edit");
    assert_eq!(parse_toolchain_channel(&out).as_deref(), Some("1.96.0"));
    let out = set_channel("# nothing yet\n", "1.96.0").expect("edit");
    assert!(out.starts_with("# nothing yet\n"));
    assert_eq!(parse_toolchain_channel(&out).as_deref(), Some("1.96.0"));
}

#[test]
fn refuses_shapes_it_cannot_edit_safely() {
    // A dotted key: appending a `[toolchain]` table would redefine it.
    let err = set_channel("toolchain.channel = \"1.95.0\"\n", "1.96.0").expect_err("dotted");
    assert!(matches!(err, SyncError::PinEdit { .. }), "{err:?}");
}

#[test]
fn creates_rust_toolchain_toml_in_unpinned_cwd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_pin(dir.path(), "1.96.0").expect("write");
    assert_eq!(path, dir.path().join("rust-toolchain.toml"));
    let text = std::fs::read_to_string(&path).expect("read");
    assert_eq!(parse_toolchain_channel(&text).as_deref(), Some("1.96.0"));
}

#[test]
fn updates_the_nearest_existing_pin_in_an_ancestor() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sub = dir.path().join("crates/a");
    std::fs::create_dir_all(&sub).expect("mkdir");
    let pin = dir.path().join("rust-toolchain.toml");
    std::fs::write(&pin, "[toolchain]\nchannel = \"1.95.0\"\n").expect("write");
    let path = write_pin(&sub, "1.96.0").expect("write");
    assert_eq!(path, pin);
    assert!(!sub.join("rust-toolchain.toml").exists(), "no second pin");
    let text = std::fs::read_to_string(&pin).expect("read");
    assert_eq!(parse_toolchain_channel(&text).as_deref(), Some("1.96.0"));
}

#[test]
fn rewrites_legacy_plain_rust_toolchain_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy = dir.path().join("rust-toolchain");
    std::fs::write(&legacy, "1.95.0\n").expect("write");
    let path = write_pin(dir.path(), "1.96.0").expect("write");
    assert_eq!(path, legacy);
    assert_eq!(std::fs::read_to_string(&legacy).expect("read"), "1.96.0\n");
}

#[test]
fn edits_legacy_file_that_holds_toml() {
    let dir = tempfile::tempdir().expect("tempdir");
    let legacy = dir.path().join("rust-toolchain");
    std::fs::write(&legacy, "[toolchain]\nchannel = \"1.95.0\"\n").expect("write");
    write_pin(dir.path(), "1.96.0").expect("write");
    let text = std::fs::read_to_string(&legacy).expect("read");
    assert_eq!(parse_toolchain_channel(&text).as_deref(), Some("1.96.0"));
}

#[test]
fn reads_the_pinned_channel() {
    use super::pin::pinned_channel;
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(pinned_channel(dir.path()), None);
    std::fs::write(dir.path().join("rust-toolchain"), "1.95.0\n").expect("write");
    assert_eq!(pinned_channel(dir.path()).as_deref(), Some("1.95.0"));
    let toml = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        toml.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"nightly-2026-05-28\"\n",
    )
    .expect("write");
    assert_eq!(
        pinned_channel(toml.path()).as_deref(),
        Some("nightly-2026-05-28")
    );
    std::fs::write(
        toml.path().join("rust-toolchain.toml"),
        "[toolchain]\npath = \"/x\"\n",
    )
    .expect("write");
    assert_eq!(
        pinned_channel(toml.path()),
        None,
        "a path toolchain has no channel"
    );
}
