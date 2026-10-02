//! `rbs doctor --sync-toolchain[=newest|oldest]`: converge this host and the
//! remote on one toolchain for a workspace. See
//! `docs/features/toolchain-sync.md`.
//!
//! Order matters: install on both hosts, verify both resolve the same rustc
//! build, and only then write the pin, so a failure never leaves a pin that
//! one host cannot satisfy.

mod pin;
mod release;

#[cfg(test)]
mod pin_tests;
#[cfg(test)]
mod release_tests;

use std::path::{Path, PathBuf};

use rbs_toolchain::{ToolchainFingerprint, parse_rustc_vv};
use thiserror::Error;
use tracing::info;

use crate::bootstrap::{login_shell_args, shell_quote, toolchain_installed};
use crate::runner::{Output, Runner, RunnerError};

pub use pin::pinned_channel;
use pin::write_pin;
use release::{Channel, Winner};
pub use release::{SyncPolicy, commit_date};

/// Label for this host in reports.
const LOCAL: &str = "local";

/// One host's toolchain as probed for the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub fp: ToolchainFingerprint,
    /// `commit-date` from `rustc -vV`; orders nightlies of the same version.
    pub commit_date: Option<String>,
}

/// Where the remote's toolchain for the workspace was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteView {
    /// The remote mirrors `cwd` (same absolute path); probed inside it.
    Mirror,
    /// No mirror yet, and the workspace pins this channel — what a remote
    /// build resolves once rsync brings the pin over.
    Pinned(String),
    /// No mirror and no pin: the remote's default toolchain.
    Default,
}

impl RemoteView {
    /// Report suffix explaining a probe made without a mirror.
    pub fn note(&self, host: &str, cwd: &Path) -> Option<String> {
        let what = match self {
            RemoteView::Mirror => return None,
            RemoteView::Pinned(ch) => format!("its pinned {ch}"),
            RemoteView::Default => "its default toolchain".to_string(),
        };
        Some(format!(
            "no mirror of {} on {host} yet; checked {what}",
            cwd.display()
        ))
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(
        "cannot order rustc release `{release}` (expected X.Y.Z, X.Y.Z-beta.N or X.Y.Z-nightly)"
    )]
    UnparsableRelease { release: String },
    #[error(
        "host triples differ (local {local}, remote {remote}); one toolchain cannot serve both"
    )]
    HostTripleMismatch { local: String, remote: String },
    #[error(
        "both hosts report rustc {release} with different commits; neither is newer — reinstall it on one host"
    )]
    Undecidable { release: String },
    #[error(
        "{host} runs a floating toolchain `{name}`; pin `{prefix}-YYYY-MM-DD` there (e.g. nightly-YYYY-MM-DD) so it can be reproduced"
    )]
    FloatingChannel {
        host: String,
        name: String,
        prefix: &'static str,
    },
    #[error("{host}: `{command}` failed: {detail}")]
    Command {
        host: String,
        command: String,
        detail: String,
    },
    #[error("{host}: cannot parse `rustc +{channel} -vV`: {detail}")]
    Verify {
        host: String,
        channel: String,
        detail: String,
    },
    #[error(
        "installed {channel} on both hosts but they resolve different builds\n    local:  {local}\n    {host}: {remote}"
    )]
    Diverged {
        host: String,
        channel: String,
        local: Box<ToolchainFingerprint>,
        remote: Box<ToolchainFingerprint>,
    },
    #[error("cannot write {}: {source}", path.display())]
    PinIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "cannot set `[toolchain] channel = \"{channel}\"` safely; edit the toolchain file by hand"
    )]
    PinEdit { channel: String },
}

/// What a sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The fingerprints were already compatible; nothing was touched.
    AlreadyInSync,
    Synced(Box<Synced>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub policy: SyncPolicy,
    pub channel: String,
    pub winner: Winner,
    /// Host labels where the channel was newly installed.
    pub installed: Vec<String>,
    pub pin: PathBuf,
    /// Whether the pin was copied to the remote's mirror of the workspace
    /// (false when the remote has no mirror yet).
    pub pin_pushed: bool,
    /// Both hosts' fingerprints for the pinned channel after the sync.
    pub local: ToolchainFingerprint,
    pub remote: ToolchainFingerprint,
}

impl Synced {
    pub fn describe(&self, host: &str) -> String {
        let from = match self.winner {
            Winner::Local => LOCAL,
            Winner::Remote => host,
        };
        let installed = if self.installed.is_empty() {
            "already installed on both hosts".to_string()
        } else {
            format!("installed on {}", self.installed.join(", "))
        };
        let pushed = if self.pin_pushed {
            format!("copied the pin to {host}")
        } else {
            format!(
                "{host} has no mirror of this workspace yet; the next remote build carries the pin"
            )
        };
        format!(
            "synced to {} ({}, from {from}); {installed}; pinned in {}; {pushed}",
            self.channel,
            self.policy,
            self.pin.display()
        )
    }
}

/// Converge `local` and `host`'s `remote` toolchain for `cwd` under `policy`.
pub fn sync(
    runner: &dyn Runner,
    host: &str,
    cwd: &Path,
    policy: SyncPolicy,
    local: &Probe,
    remote: &Probe,
    view: &RemoteView,
) -> Result<Outcome, SyncError> {
    if local.fp.compatible_with(&remote.fp) {
        return Ok(Outcome::AlreadyInSync);
    }
    let target = release::choose(policy, local, remote)?;
    let (winner_host, rustup_name) = match (target.winner, target.release.channel) {
        (Winner::Local, _) => (LOCAL, local.fp.toolchain_name.clone()),
        // Stable pins the release number; the rustup name is not consulted.
        (Winner::Remote, Channel::Stable) => (host, String::new()),
        (Winner::Remote, _) => (host, remote_active_toolchain(runner, host, cwd, view)?),
    };
    let channel = release::pin_channel(&target.release, &rustup_name, &local.fp.host, winner_host)?;
    info!(host, %policy, channel, winner = winner_host, "toolchain sync: target chosen");

    let mut installed = Vec::new();
    if ensure_installed(&Host::Local, runner, &channel)? {
        installed.push(LOCAL.to_string());
    }
    if ensure_installed(&Host::Remote(host), runner, &channel)? {
        installed.push(host.to_string());
    }

    let local_fp = verify(&Host::Local, runner, &channel)?;
    let remote_fp = verify(&Host::Remote(host), runner, &channel)?;
    if !local_fp.compatible_with(&remote_fp) {
        return Err(SyncError::Diverged {
            host: host.to_string(),
            channel,
            local: Box::new(local_fp),
            remote: Box::new(remote_fp),
        });
    }

    let pin = write_pin(cwd, &channel)?;
    info!(pin = %pin.display(), channel, "toolchain sync: pinned");
    let pin_pushed = push_pin(runner, host, &pin)?;
    Ok(Outcome::Synced(Box::new(Synced {
        policy,
        channel,
        winner: target.winner,
        installed,
        pin,
        pin_pushed,
        local: local_fp,
        remote: remote_fp,
    })))
}

/// Where a command runs. Remote commands go through a login shell, where
/// rustup's `~/.cargo/bin` is on PATH (see [`login_shell_args`]).
enum Host<'a> {
    Local,
    Remote(&'a str),
}

impl Host<'_> {
    fn label(&self) -> &str {
        match self {
            Host::Local => LOCAL,
            Host::Remote(h) => h,
        }
    }

    /// Run `argv` (a plain word list with no shell metacharacters).
    fn run(&self, runner: &dyn Runner, argv: &[&str]) -> Result<String, SyncError> {
        let command = argv.join(" ");
        let res: Result<Output, RunnerError> = match self {
            Host::Local => runner.run(argv[0], &argv[1..]),
            Host::Remote(h) => {
                let args = login_shell_args(h, &command);
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                runner.run("ssh", &args)
            }
        };
        let fail = |detail: String| SyncError::Command {
            host: self.label().to_string(),
            command: command.clone(),
            detail,
        };
        match res {
            Ok(out) if out.success() => Ok(out.stdout),
            Ok(out) => Err(fail(out.failure_detail())),
            Err(e) => Err(fail(e.to_string())),
        }
    }
}

/// Install `channel` unless present. Returns whether an install ran.
fn ensure_installed(host: &Host, runner: &dyn Runner, channel: &str) -> Result<bool, SyncError> {
    let list = host.run(runner, &["rustup", "toolchain", "list"])?;
    if toolchain_installed(&list, channel) {
        return Ok(false);
    }
    host.run(
        runner,
        &[
            "rustup",
            "toolchain",
            "install",
            channel,
            "--profile",
            "minimal",
        ],
    )?;
    info!(host = host.label(), channel, "toolchain sync: installed");
    Ok(true)
}

/// Fingerprint `channel` explicitly (`rustc +channel`), independent of any
/// pin or directory override.
fn verify(
    host: &Host,
    runner: &dyn Runner,
    channel: &str,
) -> Result<ToolchainFingerprint, SyncError> {
    let plus = format!("+{channel}");
    let out = host.run(runner, &["rustc", &plus, "-vV"])?;
    let vv = parse_rustc_vv(&out).map_err(|e| SyncError::Verify {
        host: host.label().to_string(),
        channel: channel.to_string(),
        detail: e.to_string(),
    })?;
    Ok(ToolchainFingerprint {
        rustc_commit: vv.commit_hash,
        rustc_version: vv.release,
        host: vv.host,
        cargo_version: String::new(),
        toolchain_name: channel.to_string(),
    })
}

/// The remote's active rustup toolchain name in its mirror of `cwd`.
fn remote_active_toolchain(
    runner: &dyn Runner,
    host: &str,
    cwd: &Path,
    view: &RemoteView,
) -> Result<String, SyncError> {
    let command = match view {
        RemoteView::Mirror => format!(
            "cd {} && rustup show active-toolchain",
            shell_quote(&cwd.display().to_string())
        ),
        RemoteView::Pinned(channel) => return Ok(channel.clone()),
        RemoteView::Default => "rustup show active-toolchain".to_string(),
    };
    let args = login_shell_args(host, &command);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let fail = |detail: String| SyncError::Command {
        host: host.to_string(),
        command: command.clone(),
        detail,
    };
    match runner.run("ssh", &args) {
        Ok(out) if out.success() => Ok(out
            .stdout
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()),
        Ok(out) => Err(fail(out.failure_detail())),
        Err(e) => Err(fail(e.to_string())),
    }
}

/// Copy `pin` to the same path on `host` when the remote already mirrors its
/// directory (rbs mirrors worktrees at identical absolute paths). Returns
/// false when there is no mirror yet; the next remote build's rsync brings it.
fn push_pin(runner: &dyn Runner, host: &str, pin: &Path) -> Result<bool, SyncError> {
    let Some(dir) = pin.parent() else {
        return Ok(false);
    };
    let dir = shell_quote(&dir.display().to_string());
    let mirrored = runner
        .run("ssh", &[host, "test", "-d", &dir])
        .is_ok_and(|o| o.success());
    if !mirrored {
        return Ok(false);
    }
    let src = pin.display().to_string();
    let dst = format!("{host}:{src}");
    match runner.run("scp", &["-q", &src, &dst]) {
        Ok(out) if out.success() => Ok(true),
        Ok(out) => Err(SyncError::Command {
            host: host.to_string(),
            command: format!("scp {src} {dst}"),
            detail: out.failure_detail(),
        }),
        Err(e) => Err(SyncError::Command {
            host: host.to_string(),
            command: format!("scp {src} {dst}"),
            detail: e.to_string(),
        }),
    }
}
