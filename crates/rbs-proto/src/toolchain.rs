use serde::{Deserialize, Serialize};
use std::fmt;

/// Identity of the Rust toolchain active for a workspace on one host.
///
/// Two fingerprints are *compatible* (produce identical kache keys for libs)
/// when `rustc_commit` and `host` agree; the other fields are informational.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainFingerprint {
    /// `commit-hash` from `rustc -vV` (short or full).
    pub rustc_commit: String,
    /// `release` from `rustc -vV`, e.g. `1.95.0` or `1.100.0-nightly`.
    pub rustc_version: String,
    /// `host` from `rustc -vV`.
    pub host: String,
    /// `cargo -V` output, e.g. `cargo 1.95.0 (…)`.
    pub cargo_version: String,
    /// `rustup show active-toolchain` name; empty when rustup is absent.
    pub toolchain_name: String,
}

impl ToolchainFingerprint {
    pub fn compatible_with(&self, other: &ToolchainFingerprint) -> bool {
        self.rustc_commit == other.rustc_commit && self.host == other.host
    }

    /// Nightly builds without a date pin drift daily; worth a hint in errors.
    pub fn is_nightly(&self) -> bool {
        self.rustc_version.contains("nightly")
    }
}

impl fmt::Display for ToolchainFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rustc {} ({}) {}",
            self.rustc_version, self.rustc_commit, self.host
        )?;
        if !self.toolchain_name.is_empty() {
            write!(f, " [{}]", self.toolchain_name)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(commit: &str, host: &str) -> ToolchainFingerprint {
        ToolchainFingerprint {
            rustc_commit: commit.into(),
            rustc_version: "1.95.0".into(),
            host: host.into(),
            cargo_version: "cargo 1.95.0".into(),
            toolchain_name: String::new(),
        }
    }

    #[test]
    fn compatibility_is_commit_and_host() {
        let a = fp("abc", "x86_64-unknown-linux-gnu");
        assert!(a.compatible_with(&fp("abc", "x86_64-unknown-linux-gnu")));
        assert!(!a.compatible_with(&fp("def", "x86_64-unknown-linux-gnu")));
        assert!(!a.compatible_with(&fp("abc", "aarch64-unknown-linux-gnu")));
        let mut b = fp("abc", "x86_64-unknown-linux-gnu");
        b.cargo_version = "cargo 1.95.0 (other)".into();
        b.toolchain_name = "stable".into();
        assert!(
            a.compatible_with(&b),
            "informational fields must not matter"
        );
    }

    #[test]
    fn display_includes_name_when_present() {
        let mut a = fp("abc", "h");
        assert_eq!(a.to_string(), "rustc 1.95.0 (abc) h");
        a.toolchain_name = "1.95.0-h".into();
        assert_eq!(a.to_string(), "rustc 1.95.0 (abc) h [1.95.0-h]");
        a.rustc_version = "1.100.0-nightly".into();
        assert!(a.is_nightly());
    }
}
