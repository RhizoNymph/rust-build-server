//! Environment overrides and the job-environment allowlist.

use std::collections::BTreeMap;

use rbs_proto::Priority;

use crate::{Config, ConfigError, Mode};

/// Values read from `RBS_*` variables. Applied last, over every file layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvOverrides {
    pub mode: Option<Mode>,
    pub remote_host: Option<String>,
    pub priority: Option<Priority>,
}

impl EnvOverrides {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Testable core: `lookup` returns the variable's value if set.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut o = EnvOverrides::default();
        if let Some(m) = lookup("RBS_MODE") {
            o.mode = Some(m.parse().map_err(|message| ConfigError::Env {
                var: "RBS_MODE",
                message,
            })?);
        }
        // RBS_LOCAL=1 is shorthand for RBS_MODE=local; RBS_MODE wins if both are set.
        if o.mode.is_none() && lookup("RBS_LOCAL").is_some_and(|v| is_truthy(&v)) {
            o.mode = Some(Mode::Local);
        }
        if let Some(h) = lookup("RBS_REMOTE_HOST").filter(|h| !h.is_empty()) {
            o.remote_host = Some(h);
        }
        if let Some(p) = lookup("RBS_PRIORITY") {
            o.priority = Some(p.parse().map_err(|message| ConfigError::Env {
                var: "RBS_PRIORITY",
                message,
            })?);
        }
        Ok(o)
    }

    pub fn apply(&self, cfg: &mut Config) {
        if let Some(m) = self.mode {
            cfg.policy.mode = m;
        }
        if let Some(h) = &self.remote_host {
            cfg.remote.host = h.clone();
        }
        if let Some(p) = self.priority {
            cfg.policy.priority = p;
        }
    }
}

fn is_truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Does `name` match a pattern from the allowlist? Patterns are literal names or
/// a literal prefix followed by a single trailing `*`.
pub fn env_allowed(name: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|pat| match pat.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pat,
    })
}

/// Variables that must never be forwarded to a job even if an allowlist pattern
/// would admit them: they describe the *client* host, not the build.
const NEVER_FORWARD: &[&str] = &[
    "RUSTUP_HOME",
    "CARGO_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_TARGET_DIR",
];

/// Filter a process environment down to what should travel with a job.
pub fn filter_env<'a>(
    vars: impl IntoIterator<Item = (&'a str, &'a str)>,
    allowlist: &[String],
) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(k, _)| !NEVER_FORWARD.contains(k) && env_allowed(k, allowlist))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn rbs_mode_parses_and_beats_rbs_local() {
        let o = EnvOverrides::from_lookup(lookup(&[("RBS_MODE", "plain"), ("RBS_LOCAL", "1")]))
            .expect("ok");
        assert_eq!(o.mode, Some(Mode::Plain));
        let o = EnvOverrides::from_lookup(lookup(&[("RBS_LOCAL", "yes")])).expect("ok");
        assert_eq!(o.mode, Some(Mode::Local));
        let o = EnvOverrides::from_lookup(lookup(&[("RBS_LOCAL", "0")])).expect("ok");
        assert_eq!(o.mode, None);
    }

    #[test]
    fn invalid_values_are_typed_errors() {
        let err =
            EnvOverrides::from_lookup(lookup(&[("RBS_MODE", "cloud")])).expect_err("must fail");
        assert!(matches!(
            err,
            ConfigError::Env {
                var: "RBS_MODE",
                ..
            }
        ));
        let err =
            EnvOverrides::from_lookup(lookup(&[("RBS_PRIORITY", "max")])).expect_err("must fail");
        assert!(matches!(
            err,
            ConfigError::Env {
                var: "RBS_PRIORITY",
                ..
            }
        ));
    }

    #[test]
    fn apply_overrides_only_set_fields() {
        let mut cfg = Config::default();
        EnvOverrides {
            mode: None,
            remote_host: Some("big".into()),
            priority: Some(Priority::Interactive),
        }
        .apply(&mut cfg);
        assert_eq!(cfg.policy.mode, Mode::Auto);
        assert_eq!(cfg.remote.host, "big");
        assert_eq!(cfg.policy.priority, Priority::Interactive);
    }

    #[test]
    fn allowlist_matching() {
        let al: Vec<String> = vec!["CARGO_*".into(), "CC".into(), "RUST*".into()];
        assert!(env_allowed("CARGO_FEATURE_X", &al));
        assert!(env_allowed("CC", &al));
        assert!(env_allowed("RUSTFLAGS", &al));
        assert!(env_allowed("RUST_LOG", &al));
        assert!(!env_allowed("CCACHE_DIR", &al));
        assert!(!env_allowed("PATH", &al));
        assert!(!env_allowed("CARGO", &al), "prefix requires the underscore");
    }

    #[test]
    fn filter_env_drops_host_specific_vars() {
        let al: Vec<String> = vec!["CARGO_*".into(), "RUST*".into()];
        let out = filter_env(
            [
                ("CARGO_HOME", "/home/x/.cargo"),
                ("CARGO_TARGET_DIR", "/tmp/t"),
                ("CARGO_INCREMENTAL", "0"),
                ("RUSTFLAGS", "-Cdebuginfo=0"),
                ("RUSTUP_TOOLCHAIN", "nightly"),
                ("HOME", "/home/x"),
            ],
            &al,
        );
        assert_eq!(
            out,
            BTreeMap::from([
                ("CARGO_INCREMENTAL".to_string(), "0".to_string()),
                ("RUSTFLAGS".to_string(), "-Cdebuginfo=0".to_string()),
            ])
        );
    }
}
