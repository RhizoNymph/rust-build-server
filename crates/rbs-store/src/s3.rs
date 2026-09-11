//! Build an S3 [`object_store`] client for the kache MinIO store.

use object_store::aws::{AmazonS3, AmazonS3Builder};
use thiserror::Error;

use crate::creds::Credentials;
use crate::kache_cfg::RemoteStore;

pub const DEFAULT_REGION: &str = "us-east-1";

#[derive(Debug, Error)]
pub enum S3Error {
    #[error("building the S3 client failed: {0}")]
    Build(#[source] object_store::Error),
}

/// Path-style S3 client (MinIO does not serve virtual-hosted buckets).
pub fn build_s3(remote: &RemoteStore, creds: &Credentials) -> Result<AmazonS3, S3Error> {
    let mut builder = AmazonS3Builder::new()
        .with_bucket_name(&remote.bucket)
        .with_region(remote.region.as_deref().unwrap_or(DEFAULT_REGION))
        .with_access_key_id(&creds.access_key)
        .with_secret_access_key(&creds.secret_key)
        .with_virtual_hosted_style_request(false);
    if let Some(endpoint) = &remote.endpoint {
        builder = builder
            .with_endpoint(endpoint)
            .with_allow_http(endpoint.starts_with("http://"));
    }
    builder.build().map_err(S3Error::Build)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            access_key: "AK".into(),
            secret_key: "SK".into(),
        }
    }

    #[test]
    fn builds_for_http_minio_endpoint() {
        let remote = RemoteStore {
            bucket: "kache".into(),
            endpoint: Some("http://127.0.0.1:9100".into()),
            region: Some("us-east-1".into()),
            profile: Some("rbs".into()),
            prefix: Some("artifacts".into()),
        };
        assert!(build_s3(&remote, &creds()).is_ok());
    }

    #[test]
    fn builds_without_optional_fields() {
        let remote = RemoteStore {
            bucket: "kache".into(),
            endpoint: None,
            region: None,
            profile: None,
            prefix: None,
        };
        assert!(build_s3(&remote, &creds()).is_ok());
    }
}
