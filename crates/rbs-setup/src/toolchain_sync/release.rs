//! Ordering two hosts' toolchains and naming the one to pin.

use std::fmt;
use std::str::FromStr;

use super::{Probe, SyncError};

/// Which of two mismatched toolchains `rbs doctor --sync-toolchain` converges on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncPolicy {
    #[default]
    Newest,
    Oldest,
}

impl SyncPolicy {
    pub fn name(self) -> &'static str {
        match self {
            SyncPolicy::Newest => "newest",
            SyncPolicy::Oldest => "oldest",
        }
    }
}

impl fmt::Display for SyncPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for SyncPolicy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "newest" => Ok(SyncPolicy::Newest),
            "oldest" => Ok(SyncPolicy::Oldest),
            other => Err(format!(
                "unknown sync policy `{other}` (expected newest|oldest)"
            )),
        }
    }
}

/// Release channel. Declaration order is the ordering at an equal version:
/// `1.97.0-nightly` < `1.97.0-beta.N` < `1.97.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    Nightly,
    Beta,
    Stable,
}

/// A rustc `release` plus its `commit-date`. Field order is the total order:
/// version, then channel, then commit date (tie-break between nightlies).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Release {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub channel: Channel,
    pub commit_date: Option<String>,
}

impl Release {
    pub fn parse(release: &str, commit_date: Option<&str>) -> Result<Self, SyncError> {
        let bad = || SyncError::UnparsableRelease {
            release: release.to_string(),
        };
        let (version, channel) = match release.split_once('-') {
            None => (release, Channel::Stable),
            Some((v, "nightly")) => (v, Channel::Nightly),
            Some((v, pre)) if pre.starts_with("beta") => (v, Channel::Beta),
            Some(_) => return Err(bad()),
        };
        let mut parts = version
            .split('.')
            .map(|p| p.parse::<u32>().map_err(|_| bad()));
        let mut next = || parts.next().ok_or_else(bad).and_then(|r| r);
        let (major, minor, patch) = (next()?, next()?, next()?);
        if parts.next().is_some() {
            return Err(bad());
        }
        Ok(Release {
            major,
            minor,
            patch,
            channel,
            commit_date: commit_date.map(str::to_string),
        })
    }

    fn of(probe: &Probe) -> Result<Self, SyncError> {
        Self::parse(&probe.fp.rustc_version, probe.commit_date.as_deref())
    }
}

/// `commit-date` from `rustc -vV`; `None` when absent or `unknown`.
pub fn commit_date(rustc_vv: &str) -> Option<String> {
    rustc_vv
        .lines()
        .find_map(|l| l.strip_prefix("commit-date:"))
        .map(str::trim)
        .filter(|d| !d.is_empty() && *d != "unknown")
        .map(str::to_string)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Winner {
    Local,
    Remote,
}

/// The toolchain both hosts should converge on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub winner: Winner,
    pub release: Release,
}

/// Pick the side whose toolchain `policy` prefers. The caller has already
/// established that the two fingerprints are not compatible.
pub fn choose(policy: SyncPolicy, local: &Probe, remote: &Probe) -> Result<Target, SyncError> {
    if local.fp.host != remote.fp.host {
        return Err(SyncError::HostTripleMismatch {
            local: local.fp.host.clone(),
            remote: remote.fp.host.clone(),
        });
    }
    let (l, r) = (Release::of(local)?, Release::of(remote)?);
    let winner = match (l.cmp(&r), policy) {
        (std::cmp::Ordering::Equal, _) => {
            return Err(SyncError::Undecidable {
                release: local.fp.rustc_version.clone(),
            });
        }
        (std::cmp::Ordering::Greater, SyncPolicy::Newest)
        | (std::cmp::Ordering::Less, SyncPolicy::Oldest) => Winner::Local,
        _ => Winner::Remote,
    };
    let release = match winner {
        Winner::Local => l,
        Winner::Remote => r,
    };
    Ok(Target { winner, release })
}

/// The channel string to pin and install for `release`, whose active rustup
/// toolchain on `host_label` is `rustup_name` (target triple `triple`).
///
/// Stable pins the exact release, which rustup can always install. A
/// pre-release can only be reproduced from a dated rustup name
/// (`nightly-YYYY-MM-DD`, `beta-YYYY-MM-DD`); a floating one is refused.
pub fn pin_channel(
    release: &Release,
    rustup_name: &str,
    triple: &str,
    host_label: &str,
) -> Result<String, SyncError> {
    let prefix = match release.channel {
        Channel::Stable => {
            return Ok(format!(
                "{}.{}.{}",
                release.major, release.minor, release.patch
            ));
        }
        Channel::Nightly => "nightly",
        Channel::Beta => "beta",
    };
    let name = rustup_name
        .strip_suffix(triple)
        .and_then(|n| n.strip_suffix('-'))
        .unwrap_or(rustup_name);
    let dated = name
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(is_iso_date);
    if dated {
        Ok(name.to_string())
    } else {
        Err(SyncError::FloatingChannel {
            host: host_label.to_string(),
            name: rustup_name.to_string(),
            prefix,
        })
    }
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}
