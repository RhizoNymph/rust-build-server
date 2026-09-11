//! Toolchain fingerprinting and mismatch detection.
//!
//! See `docs/features/toolchain.md`.

use std::path::Path;
use std::process::Command;

pub use rbs_proto::ToolchainFingerprint;

/// Set on every subprocess spawned here; the `cargo` shim execs the real
/// cargo immediately when it sees this variable.
pub const SHIM_GUARD_ENV: &str = "RBS_SHIM_ACTIVE";
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolchainError {
    #[error("failed to run `{program}` in {cwd}: {source}")]
    Spawn {
        program: &'static str,
        cwd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{program}` exited with {status} in {cwd}: {stderr}")]
    Failed {
        program: &'static str,
        cwd: String,
        status: String,
        stderr: String,
    },
    #[error("could not parse `rustc -vV` output: missing `{field}`")]
    Parse { field: &'static str },
}

/// Loud, structured mismatch between two hosts' toolchains.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{}", render(self))]
pub struct Mismatch {
    pub client_host: String,
    pub client: ToolchainFingerprint,
    pub server_host: String,
    pub server: ToolchainFingerprint,
}

fn render(m: &Mismatch) -> String {
    let mut s = format!(
        "toolchain mismatch between {} and {} — refusing to build\n  {:<8} {}\n  {:<8} {}\n",
        m.client_host,
        m.server_host,
        format!("{}:", m.client_host),
        m.client,
        format!("{}:", m.server_host),
        m.server
    );
    if m.client.rustc_version == m.server.rustc_version {
        s.push_str(
            "  hint: same release but different commit — one side has a different build of this version\n",
        );
    } else {
        s.push_str(&format!(
            "  hint: pin the project with rust-toolchain.toml and run `rustup toolchain install {}` on {}\n",
            m.client.rustc_version, m.server_host
        ));
    }
    if m.client.is_nightly() || m.server.is_nightly() {
        s.push_str(
            "  hint: an undated `nightly` channel drifts daily; pin `nightly-YYYY-MM-DD` instead\n",
        );
    }
    s
}

/// Compare a client's fingerprint with the server's for the same workspace.
pub fn check(
    client_host: &str,
    client: &ToolchainFingerprint,
    server_host: &str,
    server: &ToolchainFingerprint,
) -> Result<(), Box<Mismatch>> {
    if client.compatible_with(server) {
        Ok(())
    } else {
        Err(Box::new(Mismatch {
            client_host: client_host.to_string(),
            client: client.clone(),
            server_host: server_host.to_string(),
            server: server.clone(),
        }))
    }
}

/// Find the toolchain contract file (`rust-toolchain.toml` or `rust-toolchain`)
/// governing `cwd`, walking up the directory tree exactly as rustup does.
pub fn find_toolchain_file(cwd: &Path) -> Option<std::path::PathBuf> {
    cwd.ancestors().find_map(|d| {
        ["rust-toolchain.toml", "rust-toolchain"]
            .iter()
            .map(|n| d.join(n))
            .find(|p| p.is_file())
    })
}

/// Parsed view of `rustc -vV`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustcVv {
    pub release: String,
    pub commit_hash: String,
    pub host: String,
}

pub fn parse_rustc_vv(out: &str) -> Result<RustcVv, ToolchainError> {
    fn field<'a>(out: &'a str, key: &str) -> Option<&'a str> {
        out.lines()
            .find_map(|l| l.strip_prefix(key).map(|v| v.trim()))
    }
    let release = field(out, "release:").ok_or(ToolchainError::Parse { field: "release" })?;
    let host = field(out, "host:").ok_or(ToolchainError::Parse { field: "host" })?;
    // `commit-hash: unknown` happens for distro builds; keep the literal so it still compares.
    let commit_hash = field(out, "commit-hash:").ok_or(ToolchainError::Parse {
        field: "commit-hash",
    })?;
    Ok(RustcVv {
        release: release.to_string(),
        commit_hash: commit_hash.chars().take(9).collect(),
        host: host.to_string(),
    })
}

fn run(program: &'static str, args: &[&str], cwd: &Path) -> Result<String, ToolchainError> {
    let path = std::env::var("PATH").unwrap_or_default();
    run_with_path(program, args, cwd, &path)
}

fn run_with_path(
    program: &'static str,
    args: &[&str],
    cwd: &Path,
    path: &str,
) -> Result<String, ToolchainError> {
    let out = Command::new(program)
        .env("PATH", path)
        .args(args)
        .current_dir(cwd)
        // The rbs cargo shim honours this guard and execs the real cargo, so
        // fingerprinting from inside the shim can never recurse into itself.
        .env(SHIM_GUARD_ENV, "1")
        .output()
        .map_err(|source| ToolchainError::Spawn {
            program,
            cwd: cwd.display().to_string(),
            source,
        })?;
    if !out.status.success() {
        return Err(ToolchainError::Failed {
            program,
            cwd: cwd.display().to_string(),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fingerprint the toolchain that `cargo`/`rustc` resolve to inside `cwd`.
///
/// Goes through the rustup proxies on `PATH` so `rust-toolchain.toml` and
/// directory overrides are honoured exactly as cargo would.
pub fn fingerprint(cwd: &Path) -> Result<ToolchainFingerprint, ToolchainError> {
    let vv = parse_rustc_vv(&run("rustc", &["-vV"], cwd)?)?;
    let cargo_version = run("cargo", &["-V"], cwd)?.trim().to_string();
    let toolchain_name = run("rustup", &["show", "active-toolchain"], cwd)
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

#[cfg(test)]
mod tests {
    use super::*;

    const VV: &str = "rustc 1.95.0 (59807616e 2026-04-14)\nbinary: rustc\ncommit-hash: 59807616e0a1b2c3d4e5f60718293a4b5c6d7e8f\ncommit-date: 2026-04-14\nhost: x86_64-unknown-linux-gnu\nrelease: 1.95.0\nLLVM version: 21.1.0\n";

    fn fp(version: &str, commit: &str) -> ToolchainFingerprint {
        ToolchainFingerprint {
            rustc_commit: commit.into(),
            rustc_version: version.into(),
            host: "x86_64-unknown-linux-gnu".into(),
            cargo_version: format!("cargo {version}"),
            toolchain_name: format!("{version}-x86_64-unknown-linux-gnu"),
        }
    }

    #[test]
    fn finds_toolchain_file_in_ancestors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b")).expect("mkdir");
        assert_eq!(find_toolchain_file(&root.join("a/b")), None);
        std::fs::write(root.join("a/rust-toolchain.toml"), "").expect("write");
        assert_eq!(
            find_toolchain_file(&root.join("a/b")),
            Some(root.join("a/rust-toolchain.toml"))
        );
        std::fs::write(root.join("a/b/rust-toolchain"), "").expect("write");
        assert_eq!(
            find_toolchain_file(&root.join("a/b")),
            Some(root.join("a/b/rust-toolchain")),
            "nearest wins"
        );
    }

    #[test]
    fn parses_rustc_vv_and_shortens_commit() {
        let vv = parse_rustc_vv(VV).expect("parse");
        assert_eq!(
            vv,
            RustcVv {
                release: "1.95.0".into(),
                commit_hash: "59807616e".into(),
                host: "x86_64-unknown-linux-gnu".into()
            }
        );
    }

    #[test]
    fn parse_reports_missing_field() {
        let err = parse_rustc_vv("binary: rustc\nhost: x\n").expect_err("must fail");
        assert!(matches!(err, ToolchainError::Parse { field: "release" }));
    }

    #[test]
    fn check_passes_on_compatible() {
        assert_eq!(
            check(
                "laptop",
                &fp("1.95.0", "abc"),
                "node0",
                &fp("1.95.0", "abc")
            ),
            Ok(())
        );
    }

    #[test]
    fn mismatch_message_is_loud_and_specific() {
        let err = check(
            "laptop",
            &fp("1.95.0", "abc"),
            "node0",
            &fp("1.96.0", "def"),
        )
        .expect_err("must mismatch");
        let msg = err.to_string();
        assert!(msg.starts_with("toolchain mismatch between laptop and node0"));
        assert!(msg.contains("laptop:  rustc 1.95.0 (abc)"));
        assert!(msg.contains("node0:   rustc 1.96.0 (def)"));
        assert!(msg.contains("rustup toolchain install 1.95.0` on node0"));
        assert!(!msg.contains("nightly"));
    }

    #[test]
    fn mismatch_hints_for_nightly_and_same_release() {
        let err = check(
            "a",
            &fp("1.100.0-nightly", "x"),
            "b",
            &fp("1.100.0-nightly", "y"),
        )
        .expect_err("must mismatch");
        let msg = err.to_string();
        assert!(msg.contains("same release but different commit"));
        assert!(msg.contains("nightly-YYYY-MM-DD"));
    }

    #[test]
    fn subprocesses_carry_the_shim_guard() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("cargo");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$RBS_SHIM_ACTIVE\" = 1 ] || exit 99\necho cargo 9.9.9\n",
        )
        .expect("write");
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod");
        let path = format!(
            "{}:{}",
            dir.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new("cargo")
            .arg("-V")
            .env("PATH", &path)
            .env(SHIM_GUARD_ENV, "1")
            .output()
            .expect("run fake cargo");
        assert!(out.status.success(), "fake cargo must see the guard");
        let out = Command::new("cargo")
            .arg("-V")
            .env("PATH", &path)
            .env_remove(SHIM_GUARD_ENV)
            .output()
            .expect("run");
        assert_eq!(
            out.status.code(),
            Some(99),
            "sanity: fake cargo fails without guard"
        );
        let r =
            run_with_path("cargo", &["-V"], dir.path(), &path).expect("run() must set the guard");
        assert!(r.contains("9.9.9"));
    }

    #[test]
    fn fingerprint_runs_against_real_toolchain() {
        let dir = std::env::current_dir().expect("cwd");
        let fp = fingerprint(&dir).expect("fingerprint");
        assert!(!fp.rustc_commit.is_empty());
        assert!(fp.cargo_version.starts_with("cargo "));
        assert!(fp.host.contains('-'));
    }
}
