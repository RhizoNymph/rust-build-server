//! S3 credential resolution: env vars first, then an `~/.aws/credentials` profile.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::warn;

pub const ENV_ACCESS_KEY: &str = "KACHE_S3_ACCESS_KEY";
pub const ENV_SECRET_KEY: &str = "KACHE_S3_SECRET_KEY";

#[derive(Debug, Error)]
pub enum CredsError {
    #[error(
        "no S3 credentials: set {ENV_ACCESS_KEY}/{ENV_SECRET_KEY} or add profile [{profile}] to {path}"
    )]
    Missing { profile: String, path: PathBuf },
    #[error("profile [{profile}] in {path} is missing {key}")]
    Incomplete {
        profile: String,
        path: PathBuf,
        key: &'static str,
    },
}

/// S3 access credentials. Never logged or embedded in error messages.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").finish_non_exhaustive()
    }
}

/// Extract `aws_access_key_id`/`aws_secret_access_key` from an INI-style
/// credentials file section. Returns `None` when the profile header is absent,
/// key values found so far otherwise.
fn profile_keys(text: &str, profile: &str) -> Option<(Option<String>, Option<String>)> {
    let header = format!("[{profile}]");
    let mut in_section = false;
    let mut found = false;
    let (mut access, mut secret) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_section = line == header;
            found |= in_section;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().to_string();
            match k.trim() {
                "aws_access_key_id" if !v.is_empty() => access = Some(v),
                "aws_secret_access_key" if !v.is_empty() => secret = Some(v),
                _ => {}
            }
        }
    }
    found.then_some((access, secret))
}

/// Resolve credentials from an injectable env lookup and the credentials file
/// text (`None` = file absent). Env wins only when both variables are set.
pub fn resolve_credentials(
    env: impl Fn(&str) -> Option<String>,
    credentials_file: Option<&str>,
    credentials_path: &Path,
    profile: &str,
) -> Result<Credentials, CredsError> {
    let non_empty = |k: &str| env(k).filter(|v| !v.is_empty());
    match (non_empty(ENV_ACCESS_KEY), non_empty(ENV_SECRET_KEY)) {
        (Some(access_key), Some(secret_key)) => {
            return Ok(Credentials {
                access_key,
                secret_key,
            });
        }
        (None, None) => {}
        _ => warn!(
            "only one of {ENV_ACCESS_KEY}/{ENV_SECRET_KEY} is set; falling back to the aws profile"
        ),
    }
    let missing = || CredsError::Missing {
        profile: profile.to_string(),
        path: credentials_path.to_path_buf(),
    };
    let text = credentials_file.ok_or_else(missing)?;
    let (access, secret) = profile_keys(text, profile).ok_or_else(missing)?;
    let incomplete = |key| CredsError::Incomplete {
        profile: profile.to_string(),
        path: credentials_path.to_path_buf(),
        key,
    };
    Ok(Credentials {
        access_key: access.ok_or_else(|| incomplete("aws_access_key_id"))?,
        secret_key: secret.ok_or_else(|| incomplete("aws_secret_access_key"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "[default]\naws_access_key_id = D1\naws_secret_access_key = D2\n\n[rbs]\naws_access_key_id = AK\naws_secret_access_key = SK\n";

    fn path() -> PathBuf {
        PathBuf::from("/h/.aws/credentials")
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_wins_over_profile() {
        let env = |k: &str| match k {
            ENV_ACCESS_KEY => Some("EA".to_string()),
            ENV_SECRET_KEY => Some("ES".to_string()),
            _ => None,
        };
        let c = resolve_credentials(env, Some(FILE), &path(), "rbs").expect("resolve");
        assert_eq!(c.access_key, "EA");
        assert_eq!(c.secret_key, "ES");
    }

    #[test]
    fn profile_fallback_when_env_absent() {
        let c = resolve_credentials(no_env, Some(FILE), &path(), "rbs").expect("resolve");
        assert_eq!(c.access_key, "AK");
        assert_eq!(c.secret_key, "SK");
        let c = resolve_credentials(no_env, Some(FILE), &path(), "default").expect("resolve");
        assert_eq!(c.access_key, "D1");
    }

    #[test]
    fn partial_env_falls_back_to_profile() {
        let env = |k: &str| (k == ENV_ACCESS_KEY).then(|| "EA".to_string());
        let c = resolve_credentials(env, Some(FILE), &path(), "rbs").expect("resolve");
        assert_eq!(c.access_key, "AK");
    }

    #[test]
    fn empty_env_values_do_not_count() {
        let env = |_: &str| Some(String::new());
        let c = resolve_credentials(env, Some(FILE), &path(), "rbs").expect("resolve");
        assert_eq!(c.access_key, "AK");
    }

    #[test]
    fn missing_file_and_env_is_an_error() {
        let err = resolve_credentials(no_env, None, &path(), "rbs").expect_err("fail");
        assert!(matches!(err, CredsError::Missing { .. }));
        assert!(err.to_string().contains("rbs"));
    }

    #[test]
    fn missing_profile_is_an_error() {
        let err = resolve_credentials(no_env, Some(FILE), &path(), "nope").expect_err("fail");
        assert!(matches!(err, CredsError::Missing { .. }));
    }

    #[test]
    fn incomplete_profile_is_an_error() {
        let text = "[rbs]\naws_access_key_id = AK\n";
        let err = resolve_credentials(no_env, Some(text), &path(), "rbs").expect_err("fail");
        assert!(
            matches!(err, CredsError::Incomplete { key, .. } if key == "aws_secret_access_key")
        );
        assert!(!err.to_string().contains("AK"), "secrets never printed");
    }

    #[test]
    fn debug_never_reveals_secrets() {
        let c = Credentials {
            access_key: "AK".into(),
            secret_key: "SK".into(),
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("AK") && !dbg.contains("SK"));
    }
}
