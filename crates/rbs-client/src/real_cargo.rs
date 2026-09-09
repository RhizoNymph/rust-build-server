//! Locate and exec the real `cargo`, never the shim itself.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

/// Set in the environment of everything the shim runs so a nested `cargo`
/// (build scripts, `cargo run` of a tool that calls cargo, the server's job)
/// goes straight to the real binary.
pub const SHIM_ACTIVE_ENV: &str = "RBS_SHIM_ACTIVE";

#[derive(Debug, Error)]
pub enum RealCargoError {
    #[error("could not find a real `cargo` on PATH (outside the shim directory {shim_dir})")]
    NotFound { shim_dir: String },
    #[error("exec {program} failed: {source}")]
    Exec {
        program: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// True when an outer shim already owns this invocation.
pub fn shim_active() -> bool {
    std::env::var_os(SHIM_ACTIVE_ENV).is_some_and(|v| v == "1")
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let ca = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let cb = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Pure resolution. Preference order: `$CARGO_HOME/bin/cargo`, then
/// `$HOME/.cargo/bin/cargo` (rustup proxies, so `rust-toolchain.toml` is
/// honoured), then the first `cargo` on `path`. Any candidate living in
/// `shim_dir` is skipped.
pub fn resolve(
    cargo_home: Option<&Path>,
    home: Option<&Path>,
    path: &OsStr,
    shim_dir: Option<&Path>,
) -> Option<PathBuf> {
    let not_shim = |p: &Path| -> bool {
        match (p.parent(), shim_dir) {
            (Some(dir), Some(shim)) => !same_dir(dir, shim),
            _ => true,
        }
    };
    let preferred = cargo_home
        .map(|c| c.join("bin/cargo"))
        .into_iter()
        .chain(home.map(|h| h.join(".cargo/bin/cargo")));
    for cand in preferred {
        if is_executable_file(&cand) && not_shim(&cand) {
            return Some(cand);
        }
    }
    std::env::split_paths(path)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join("cargo"))
        .find(|c| is_executable_file(c) && not_shim(c))
}

/// Directory containing the running executable (the shim, when invoked as `cargo`).
pub fn self_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// Resolve using the process environment.
pub fn find() -> Result<PathBuf, RealCargoError> {
    let cargo_home = std::env::var_os("CARGO_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let shim_dir = self_dir();
    resolve(
        cargo_home.as_deref(),
        home.as_deref(),
        &path,
        shim_dir.as_deref(),
    )
    .ok_or_else(|| RealCargoError::NotFound {
        shim_dir: shim_dir
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".into()),
    })
}

/// Build the command that `exec` would run (testable without replacing the process).
pub fn command(cargo: &Path, args: &[String]) -> Command {
    let mut cmd = Command::new(cargo);
    cmd.args(args);
    if std::env::var_os("RUSTC_WRAPPER").is_none() {
        cmd.env("RUSTC_WRAPPER", "kache");
    }
    cmd.env(SHIM_ACTIVE_ENV, "1");
    cmd
}

/// Replace this process with `cargo args…`. Only returns on failure.
pub fn exec(cargo: &Path, args: &[String]) -> RealCargoError {
    use std::os::unix::process::CommandExt;
    let source = command(cargo, args).exec();
    RealCargoError::Exec {
        program: cargo.to_path_buf(),
        source,
    }
}

/// `find` + `exec`. Only returns on failure.
pub fn exec_real_cargo(args: &[String]) -> RealCargoError {
    match find() {
        Ok(cargo) => exec(&cargo, args),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[test]
    fn path_walk_skips_the_shim_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim_dir = tmp.path().join("shim");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&shim_dir).expect("mkdir");
        std::fs::create_dir_all(&real_dir).expect("mkdir");
        script(&shim_dir, "cargo");
        let real = script(&real_dir, "cargo");
        let path = std::env::join_paths([&shim_dir, &real_dir]).expect("join");
        let found = resolve(None, Some(tmp.path()), &path, Some(&shim_dir));
        assert_eq!(found, Some(real));
    }

    #[test]
    fn without_shim_dir_first_path_hit_wins() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).expect("mkdir");
        std::fs::create_dir_all(&b).expect("mkdir");
        let first = script(&a, "cargo");
        script(&b, "cargo");
        let path = std::env::join_paths([&a, &b]).expect("join");
        assert_eq!(resolve(None, None, &path, None), Some(first));
    }

    #[test]
    fn cargo_home_is_preferred_over_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ch = tmp.path().join("cargo-home");
        std::fs::create_dir_all(ch.join("bin")).expect("mkdir");
        let preferred = script(&ch.join("bin"), "cargo");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&other).expect("mkdir");
        script(&other, "cargo");
        let path = std::env::join_paths([&other]).expect("join");
        assert_eq!(resolve(Some(&ch), None, &path, None), Some(preferred));
    }

    #[test]
    fn home_dot_cargo_is_used_when_cargo_home_unset() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".cargo/bin")).expect("mkdir");
        let expected = script(&home.join(".cargo/bin"), "cargo");
        assert_eq!(
            resolve(None, Some(&home), OsStr::new(""), None),
            Some(expected)
        );
    }

    #[test]
    fn preferred_candidate_in_shim_dir_is_skipped() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ch = tmp.path().join("ch");
        std::fs::create_dir_all(ch.join("bin")).expect("mkdir");
        script(&ch.join("bin"), "cargo");
        let real_dir = tmp.path().join("real");
        std::fs::create_dir_all(&real_dir).expect("mkdir");
        let real = script(&real_dir, "cargo");
        let path = std::env::join_paths([&real_dir]).expect("join");
        assert_eq!(
            resolve(Some(&ch), None, &path, Some(&ch.join("bin"))),
            Some(real)
        );
    }

    #[test]
    fn non_executable_and_missing_are_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let d = tmp.path().join("d");
        std::fs::create_dir_all(&d).expect("mkdir");
        std::fs::write(d.join("cargo"), "not exec").expect("write");
        let path = std::env::join_paths([&d, &tmp.path().join("missing")]).expect("join");
        assert_eq!(resolve(None, None, &path, None), None);
    }

    #[test]
    fn command_sets_wrapper_and_guard() {
        let cmd = command(Path::new("/x/cargo"), &["build".into(), "--release".into()]);
        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&(SHIM_ACTIVE_ENV.to_string(), Some("1".into()))));
        if std::env::var_os("RUSTC_WRAPPER").is_none() {
            assert!(envs.contains(&("RUSTC_WRAPPER".to_string(), Some("kache".into()))));
        }
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(args, vec!["build", "--release"]);
    }
}
