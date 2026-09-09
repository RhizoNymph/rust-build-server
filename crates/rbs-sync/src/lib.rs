//! Worktree mirroring (rsync) and artifact pull (kache). See `docs/features/sync.md`.

pub mod plan;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use thiserror::Error;
use tokio::process::Command;

pub use plan::RsyncPlan;

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{program}` exited with {status}: {stderr}")]
    Failed {
        program: String,
        status: String,
        stderr: String,
    },
    #[error("could not locate workspace root from {cwd}: {reason}")]
    NoWorkspace { cwd: PathBuf, reason: String },
}

/// Run `program` with `args` in `cwd`, capturing output. Non-zero exit → `SyncError::Failed`
/// with trimmed stderr.
async fn run_captured(
    program: &str,
    args: &[std::ffi::OsString],
    cwd: Option<&Path>,
) -> Result<std::process::Output, SyncError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output().await.map_err(|source| SyncError::Spawn {
        program: program.to_string(),
        source,
    })?;
    if !out.status.success() {
        return Err(SyncError::Failed {
            program: program.to_string(),
            status: out.status.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(out)
}

/// Mirror `root` to `host:<root>` per [`RsyncPlan`].
pub async fn push(cfg: &rbs_config::Sync, root: &Path, host: &str) -> Result<(), SyncError> {
    let plan = RsyncPlan::new(cfg, root, host);
    let argv = plan.argv();
    tracing::debug!(host, root = %root.display(), argv = ?argv, "rsync push");
    let out = run_captured(&plan.rsync_path, &argv[1..], None).await?;
    tracing::debug!(
        stdout = %String::from_utf8_lossy(&out.stdout).trim(),
        "rsync finished"
    );
    Ok(())
}

/// `kache sync --pull` in `root`, fetching artifacts produced remotely.
pub async fn pull(root: &Path) -> Result<(), SyncError> {
    tracing::debug!(root = %root.display(), "kache pull");
    let args: Vec<std::ffi::OsString> = vec!["sync".into(), "--pull".into()];
    run_captured("kache", &args, Some(root)).await?;
    Ok(())
}

/// Workspace root for `cwd`: parent of `cargo locate-project --workspace`'s Cargo.toml.
pub async fn workspace_root(cwd: &Path) -> Result<PathBuf, SyncError> {
    let args: Vec<std::ffi::OsString> = vec![
        "locate-project".into(),
        "--workspace".into(),
        "--message-format".into(),
        "plain".into(),
    ];
    let out =
        run_captured("cargo", &args, Some(cwd))
            .await
            .map_err(|e| SyncError::NoWorkspace {
                cwd: cwd.to_path_buf(),
                reason: e.to_string(),
            })?;
    let manifest = String::from_utf8_lossy(&out.stdout).trim().to_string();
    manifest_to_root(Path::new(&manifest)).ok_or_else(|| SyncError::NoWorkspace {
        cwd: cwd.to_path_buf(),
        reason: format!("unexpected locate-project output `{manifest}`"),
    })
}

fn manifest_to_root(manifest: &Path) -> Option<PathBuf> {
    if manifest.as_os_str().is_empty() {
        return None;
    }
    manifest.parent().map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    /// Tests that write files and then exec them must not overlap: a write fd
    /// held open by one test can be inherited across another test's fork and
    /// trigger ETXTBSY on exec.
    static FS_LOCK: Mutex<()> = Mutex::const_new(());

    fn write_script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).expect("write");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[test]
    fn root_is_parent_of_manifest() {
        assert_eq!(
            manifest_to_root(Path::new("/w/Cargo.toml")),
            Some(PathBuf::from("/w"))
        );
        assert_eq!(manifest_to_root(Path::new("")), None);
    }

    #[tokio::test]
    async fn workspace_root_of_a_real_workspace() {
        let _g = FS_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canon");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .expect("write");
        std::fs::create_dir_all(root.join("src/deep")).expect("mkdir");
        std::fs::write(root.join("src/main.rs"), "fn main(){}").expect("write");
        let found = workspace_root(&root.join("src/deep")).await.expect("root");
        assert_eq!(found, root);
    }

    #[tokio::test]
    async fn workspace_root_errors_outside_a_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = workspace_root(dir.path()).await.expect_err("no workspace");
        assert!(matches!(err, SyncError::NoWorkspace { .. }));
    }

    #[tokio::test]
    async fn push_reports_rsync_failure_with_stderr() {
        // A fake rsync that fails loudly; no network involved.
        let _g = FS_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("rsync");
        write_script(
            &fake,
            "#!/bin/sh\necho 'boom: host unreachable' >&2\nexit 12\n",
        );
        let cfg = rbs_config::Sync {
            rsync_path: fake.display().to_string(),
            ..Default::default()
        };
        let err = push(&cfg, dir.path(), "nowhere")
            .await
            .expect_err("must fail");
        match err {
            SyncError::Failed { stderr, status, .. } => {
                assert_eq!(stderr, "boom: host unreachable");
                assert!(status.contains("12"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn push_succeeds_when_rsync_exits_zero() {
        let _g = FS_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("rsync");
        write_script(&fake, "#!/bin/sh\nexit 0\n");
        let cfg = rbs_config::Sync {
            rsync_path: fake.display().to_string(),
            ..Default::default()
        };
        push(&cfg, dir.path(), "nowhere").await.expect("ok");
    }

    #[tokio::test]
    async fn spawn_failure_is_typed() {
        let cfg = rbs_config::Sync {
            rsync_path: "/nonexistent/rsync-xyz".into(),
            ..Default::default()
        };
        let err = push(&cfg, Path::new("/tmp"), "h").await.expect_err("spawn");
        assert!(matches!(err, SyncError::Spawn { .. }));
    }
}
