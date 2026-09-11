//! Parse kache's `~/.config/kache/config.toml` to find the shared S3 store.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KacheConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid kache config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("{path} has no [cache.remote] section; run `rbs setup` first")]
    MissingRemote { path: PathBuf },
    #[error("{path}: [cache.remote] type is {found:?}, expected \"s3\"")]
    NotS3 { path: PathBuf, found: String },
    #[error("{path}: [cache.remote] is missing `bucket`")]
    MissingBucket { path: PathBuf },
}

/// The `[cache.remote]` S3 settings kache uses; only `bucket` is mandatory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteStore {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    pub prefix: Option<String>,
}

#[derive(Debug, Deserialize)]
struct KacheFile {
    cache: Option<CacheSection>,
}

#[derive(Debug, Deserialize)]
struct CacheSection {
    remote: Option<RemoteSection>,
}

#[derive(Debug, Deserialize)]
struct RemoteSection {
    #[serde(rename = "type")]
    kind: Option<String>,
    bucket: Option<String>,
    endpoint: Option<String>,
    region: Option<String>,
    profile: Option<String>,
    prefix: Option<String>,
}

/// Parse the kache config text; `path` is only used in error messages.
/// Unknown keys are tolerated (kache owns this file).
pub fn parse_kache_config(path: &Path, text: &str) -> Result<RemoteStore, KacheConfigError> {
    let file: KacheFile = toml::from_str(text).map_err(|e| KacheConfigError::Parse {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    let remote =
        file.cache
            .and_then(|c| c.remote)
            .ok_or_else(|| KacheConfigError::MissingRemote {
                path: path.to_path_buf(),
            })?;
    let kind = remote.kind.unwrap_or_default();
    if kind != "s3" {
        return Err(KacheConfigError::NotS3 {
            path: path.to_path_buf(),
            found: kind,
        });
    }
    let bucket =
        remote
            .bucket
            .filter(|b| !b.is_empty())
            .ok_or_else(|| KacheConfigError::MissingBucket {
                path: path.to_path_buf(),
            })?;
    Ok(RemoteStore {
        bucket,
        endpoint: remote.endpoint,
        region: remote.region,
        profile: remote.profile,
        prefix: remote.prefix,
    })
}

/// Load and parse the kache config from disk.
pub fn load_kache_config(path: &Path) -> Result<RemoteStore, KacheConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| KacheConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse_kache_config(path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "[cache.remote]\ntype = \"s3\"\nbucket = \"kache\"\nendpoint = \"http://127.0.0.1:9100\"\nregion = \"us-east-1\"\nprofile = \"rbs\"\nprefix = \"artifacts\"\n";

    fn p() -> &'static Path {
        Path::new("kache.toml")
    }

    #[test]
    fn parses_full_config() {
        let r = parse_kache_config(p(), FULL).expect("parse");
        assert_eq!(
            r,
            RemoteStore {
                bucket: "kache".into(),
                endpoint: Some("http://127.0.0.1:9100".into()),
                region: Some("us-east-1".into()),
                profile: Some("rbs".into()),
                prefix: Some("artifacts".into()),
            }
        );
    }

    #[test]
    fn missing_optionals_are_none() {
        let r = parse_kache_config(p(), "[cache.remote]\ntype = \"s3\"\nbucket = \"b\"\n")
            .expect("parse");
        assert_eq!(r.bucket, "b");
        assert_eq!(r.endpoint, None);
        assert_eq!(r.region, None);
        assert_eq!(r.profile, None);
        assert_eq!(r.prefix, None);
    }

    #[test]
    fn unknown_keys_are_tolerated() {
        let text = format!("{FULL}future_knob = 3\n[cache.local]\npath = \"/x\"\n");
        assert!(parse_kache_config(p(), &text).is_ok());
    }

    #[test]
    fn non_s3_type_is_an_error() {
        let err = parse_kache_config(p(), "[cache.remote]\ntype = \"gcs\"\nbucket = \"b\"\n")
            .expect_err("must fail");
        assert!(matches!(err, KacheConfigError::NotS3 { ref found, .. } if found == "gcs"));
        assert!(err.to_string().contains("kache.toml"));
    }

    #[test]
    fn missing_type_is_not_s3() {
        let err =
            parse_kache_config(p(), "[cache.remote]\nbucket = \"b\"\n").expect_err("must fail");
        assert!(matches!(err, KacheConfigError::NotS3 { .. }));
    }

    #[test]
    fn missing_remote_section_is_an_error() {
        let err = parse_kache_config(p(), "[cache.local]\npath = \"/x\"\n").expect_err("fail");
        assert!(matches!(err, KacheConfigError::MissingRemote { .. }));
        let err = parse_kache_config(p(), "").expect_err("fail");
        assert!(matches!(err, KacheConfigError::MissingRemote { .. }));
    }

    #[test]
    fn missing_or_empty_bucket_is_an_error() {
        let err = parse_kache_config(p(), "[cache.remote]\ntype = \"s3\"\n").expect_err("fail");
        assert!(matches!(err, KacheConfigError::MissingBucket { .. }));
        let err = parse_kache_config(p(), "[cache.remote]\ntype = \"s3\"\nbucket = \"\"\n")
            .expect_err("fail");
        assert!(matches!(err, KacheConfigError::MissingBucket { .. }));
    }

    #[test]
    fn malformed_toml_is_a_parse_error() {
        let err = parse_kache_config(p(), "[cache.remote\n").expect_err("fail");
        assert!(matches!(err, KacheConfigError::Parse { .. }));
    }
}
