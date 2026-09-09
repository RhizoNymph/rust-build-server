//! File layers and their merge.
//!
//! Merging is done at the TOML table level (later layers override keys
//! individually, not whole sections) and then deserialized once, so
//! `deny_unknown_fields` still applies to the combined document.

use std::path::{Path, PathBuf};

use crate::{Config, ConfigError};

#[derive(Debug, Clone)]
pub struct Layer {
    pub path: PathBuf,
    pub table: toml::Table,
}

impl Layer {
    pub fn parse(path: &Path, text: &str) -> Result<Layer, ConfigError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Parse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        Ok(Layer {
            path: path.to_path_buf(),
            table,
        })
    }

    /// `Ok(None)` when the file does not exist; other I/O errors are reported.
    pub fn read_optional(path: &Path) -> Result<Option<Layer>, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Layer::parse(path, &text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

fn deep_merge(base: &mut toml::Table, over: &toml::Table) {
    for (k, v) in over {
        match (base.get_mut(k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => deep_merge(b, o),
            _ => {
                base.insert(k.clone(), v.clone());
            }
        }
    }
}

/// Merge layers in order (last wins) and deserialize into a [`Config`].
pub fn merge_layers(layers: &[Layer]) -> Result<Config, ConfigError> {
    let mut merged = toml::Table::new();
    for l in layers {
        deep_merge(&mut merged, &l.table);
    }
    let path = layers
        .last()
        .map(|l| l.path.clone())
        .unwrap_or_else(|| PathBuf::from("<defaults>"));
    merged
        .try_into()
        .map_err(|e: toml::de::Error| ConfigError::Parse {
            path,
            message: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mode;

    fn layer(name: &str, text: &str) -> Layer {
        Layer::parse(Path::new(name), text).expect("parse layer")
    }

    #[test]
    fn later_layer_overrides_individual_keys_only() {
        let user = layer(
            "user",
            "[remote]\nhost = \"node0\"\nmax_rtt_ms = 40\n[policy]\nmode = \"auto\"\n",
        );
        let proj = layer("proj", "[remote]\nmax_rtt_ms = 200\n");
        let c = merge_layers(&[user, proj]).expect("merge");
        assert_eq!(c.remote.host, "node0", "untouched key survives");
        assert_eq!(c.remote.max_rtt_ms, 200, "overridden key wins");
        assert_eq!(c.policy.mode, Mode::Auto);
    }

    #[test]
    fn arrays_replace_rather_than_append() {
        let a = layer("a", "[policy]\nlocal_subcommands = [\"run\", \"bench\"]\n");
        let b = layer("b", "[policy]\nlocal_subcommands = [\"test\"]\n");
        let c = merge_layers(&[a, b]).expect("merge");
        assert_eq!(c.policy.local_subcommands, vec!["test"]);
    }

    #[test]
    fn empty_layers_yield_defaults() {
        assert_eq!(merge_layers(&[]).expect("merge"), Config::default());
    }

    #[test]
    fn unknown_key_in_any_layer_is_an_error_naming_last_file() {
        let a = layer("a", "[remote]\nhots = \"x\"\n");
        let err = merge_layers(&[a]).expect_err("must fail");
        assert!(err.to_string().contains("hots"));
    }

    #[test]
    fn read_optional_distinguishes_missing_from_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            Layer::read_optional(&dir.path().join("nope.toml"))
                .expect("missing ok")
                .is_none()
        );
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "= nonsense").expect("write");
        assert!(matches!(
            Layer::read_optional(&bad),
            Err(ConfigError::Parse { .. })
        ));
    }
}
