//! Backend selection: a pure decision table from (mode, probe, local availability,
//! config) to an ordered chain of backends to attempt, plus why others were skipped.

use std::fmt;
use std::time::Duration;

use rbs_config::{Config, Mode};

use crate::transport::ProbeResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Backend {
    Remote,
    Local,
    Plain,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Backend::Remote => "remote",
            Backend::Local => "local",
            Backend::Plain => "plain",
        })
    }
}

/// Why a backend was left out of the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Disabled in config (`remote.enabled` / `local_server.enabled`).
    Disabled,
    /// Probe failed (ssh error, timeout, protocol error); carries the message.
    ProbeFailed(String),
    /// Measured RTT above `remote.max_rtt_ms`.
    RttTooHigh { rtt: Duration, max: Duration },
    /// Server reported `capacity.accepting == false`.
    NotAccepting { queued: usize, running: usize },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::Disabled => f.write_str("disabled in config"),
            SkipReason::ProbeFailed(m) => write!(f, "probe failed: {m}"),
            SkipReason::RttTooHigh { rtt, max } => write!(
                f,
                "link too slow: rtt {}ms > max_rtt_ms {}",
                rtt.as_millis(),
                max.as_millis()
            ),
            SkipReason::NotAccepting { queued, running } => write!(
                f,
                "server not accepting (queued {queued}, running {running})"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub backend: Backend,
    pub reason: SkipReason,
}

/// Ordered backends to try. `chain` is empty only when a forced mode's backend
/// is unavailable; `skipped` then names it and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub chain: Vec<Backend>,
    pub skipped: Vec<Skip>,
}

impl Decision {
    #[cfg(test)]
    pub fn first(&self) -> Option<Backend> {
        self.chain.first().copied()
    }
}

/// Outcome of probing the remote, as seen by the decision table.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteProbe {
    Ok(ProbeResult),
    Failed(String),
}

/// Evaluate the remote row of the table: `None` means remote is usable.
fn remote_skip(remote: Option<&RemoteProbe>, cfg: &Config) -> Option<SkipReason> {
    if !cfg.remote.enabled {
        return Some(SkipReason::Disabled);
    }
    match remote {
        None => Some(SkipReason::ProbeFailed("not probed".into())),
        Some(RemoteProbe::Failed(m)) => Some(SkipReason::ProbeFailed(m.clone())),
        Some(RemoteProbe::Ok(p)) => {
            let max = Duration::from_millis(cfg.remote.max_rtt_ms);
            if p.rtt > max {
                return Some(SkipReason::RttTooHigh { rtt: p.rtt, max });
            }
            if !p.status.capacity.accepting {
                return Some(SkipReason::NotAccepting {
                    queued: p.status.queued,
                    running: p.status.running,
                });
            }
            None
        }
    }
}

fn local_skip(local_available: bool, cfg: &Config) -> Option<SkipReason> {
    if !cfg.local_server.enabled || !local_available {
        Some(SkipReason::Disabled)
    } else {
        None
    }
}

/// The decision table.
///
/// | mode   | remote ok | local | chain                    |
/// |--------|-----------|-------|--------------------------|
/// | auto   | yes       | any   | remote, (local), plain   |
/// | auto   | no        | yes   | local, plain             |
/// | auto   | no        | no    | plain                    |
/// | remote | yes       | any   | remote                   |
/// | remote | no        | any   | (none) — error           |
/// | local  | any       | yes   | local                    |
/// | local  | any       | no    | (none) — error           |
/// | plain  | any       | any   | plain                    |
///
/// "remote ok" = enabled && probe succeeded && rtt <= max_rtt_ms && accepting.
/// Pure: no logging; the caller reports `skipped` at `warn`.
pub fn select(
    mode: Mode,
    remote: Option<&RemoteProbe>,
    local_available: bool,
    cfg: &Config,
) -> Decision {
    let mut chain = Vec::new();
    let mut skipped = Vec::new();
    let mut consider = |backend: Backend, skip: Option<SkipReason>| match skip {
        None => chain.push(backend),
        Some(reason) => skipped.push(Skip { backend, reason }),
    };
    match mode {
        Mode::Auto => {
            consider(Backend::Remote, remote_skip(remote, cfg));
            consider(Backend::Local, local_skip(local_available, cfg));
            consider(Backend::Plain, None);
        }
        Mode::Remote => consider(Backend::Remote, remote_skip(remote, cfg)),
        Mode::Local => consider(Backend::Local, local_skip(local_available, cfg)),
        Mode::Plain => consider(Backend::Plain, None),
    }
    Decision { chain, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rbs_proto::{Capacity, ServerIdentity, ServerStatus};

    fn probe(rtt_ms: u64, accepting: bool) -> RemoteProbe {
        RemoteProbe::Ok(ProbeResult {
            status: ServerStatus {
                identity: ServerIdentity {
                    hostname: "node0".into(),
                    version: "0.1.0".into(),
                },
                capacity: Capacity {
                    tokens_total: 30,
                    tokens_free: 5,
                    mem_available_bytes: 1 << 36,
                    load1: 2.0,
                    accepting,
                },
                queued: 3,
                running: 4,
                uptime_secs: 1,
            },
            rtt: Duration::from_millis(rtt_ms),
        })
    }

    fn cfg() -> Config {
        Config::default() // max_rtt_ms = 40, remote/local enabled
    }

    fn chain(d: &Decision) -> Vec<Backend> {
        d.chain.clone()
    }

    #[test]
    fn auto_prefers_remote_when_healthy() {
        let d = select(Mode::Auto, Some(&probe(10, true)), true, &cfg());
        assert_eq!(
            chain(&d),
            vec![Backend::Remote, Backend::Local, Backend::Plain]
        );
        assert!(d.skipped.is_empty());
    }

    #[test]
    fn auto_rtt_at_limit_is_ok_but_above_is_skipped() {
        let d = select(Mode::Auto, Some(&probe(40, true)), true, &cfg());
        assert_eq!(d.first(), Some(Backend::Remote));
        let d = select(Mode::Auto, Some(&probe(41, true)), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Local, Backend::Plain]);
        assert_eq!(
            d.skipped,
            vec![Skip {
                backend: Backend::Remote,
                reason: SkipReason::RttTooHigh {
                    rtt: Duration::from_millis(41),
                    max: Duration::from_millis(40)
                }
            }]
        );
        assert!(d.skipped[0].reason.to_string().contains("41ms"));
    }

    #[test]
    fn auto_skips_remote_when_not_accepting() {
        let d = select(Mode::Auto, Some(&probe(5, false)), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Local, Backend::Plain]);
        assert_eq!(
            d.skipped[0].reason,
            SkipReason::NotAccepting {
                queued: 3,
                running: 4
            }
        );
    }

    #[test]
    fn auto_skips_remote_when_probe_failed_or_missing() {
        let failed = RemoteProbe::Failed("ssh: connect refused".into());
        let d = select(Mode::Auto, Some(&failed), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Local, Backend::Plain]);
        assert_eq!(
            d.skipped[0].reason,
            SkipReason::ProbeFailed("ssh: connect refused".into())
        );
        let d = select(Mode::Auto, None, true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Local, Backend::Plain]);
        assert!(matches!(d.skipped[0].reason, SkipReason::ProbeFailed(_)));
    }

    #[test]
    fn auto_skips_remote_when_disabled_even_if_probe_ok() {
        let mut c = cfg();
        c.remote.enabled = false;
        let d = select(Mode::Auto, Some(&probe(1, true)), true, &c);
        assert_eq!(chain(&d), vec![Backend::Local, Backend::Plain]);
        assert_eq!(d.skipped[0].reason, SkipReason::Disabled);
    }

    #[test]
    fn auto_falls_to_plain_when_nothing_else() {
        let d = select(Mode::Auto, None, false, &cfg());
        assert_eq!(chain(&d), vec![Backend::Plain]);
        assert_eq!(d.skipped.len(), 2);
        assert_eq!(d.skipped[1].backend, Backend::Local);
        assert_eq!(d.skipped[1].reason, SkipReason::Disabled);

        let mut c = cfg();
        c.local_server.enabled = false;
        let d = select(Mode::Auto, None, true, &c);
        assert_eq!(chain(&d), vec![Backend::Plain]);
    }

    #[test]
    fn auto_remote_ok_but_local_unavailable_has_no_local_in_chain() {
        let d = select(Mode::Auto, Some(&probe(1, true)), false, &cfg());
        assert_eq!(chain(&d), vec![Backend::Remote, Backend::Plain]);
    }

    #[test]
    fn forced_remote_is_remote_only_or_error() {
        let d = select(Mode::Remote, Some(&probe(1, true)), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Remote]);
        let d = select(Mode::Remote, Some(&probe(100, true)), true, &cfg());
        assert!(chain(&d).is_empty(), "no fallback in forced mode");
        assert_eq!(d.skipped[0].backend, Backend::Remote);
        let d = select(Mode::Remote, None, true, &cfg());
        assert!(chain(&d).is_empty());
    }

    #[test]
    fn forced_local_ignores_remote() {
        let d = select(Mode::Local, Some(&probe(1, true)), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Local]);
        let d = select(Mode::Local, Some(&probe(1, true)), false, &cfg());
        assert!(chain(&d).is_empty());
        assert_eq!(d.skipped[0].backend, Backend::Local);
    }

    #[test]
    fn plain_skips_everything() {
        let d = select(Mode::Plain, Some(&probe(1, true)), true, &cfg());
        assert_eq!(chain(&d), vec![Backend::Plain]);
        assert!(d.skipped.is_empty());
    }
}
