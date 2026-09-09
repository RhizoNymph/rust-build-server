//! Layered rbs configuration. See `docs/features/config.md`.
//!
//! Precedence (low → high): defaults, `~/.config/rbs/config.toml`, nearest
//! `.rbs.toml` walking up from cwd, environment variables.

mod env;
mod layers;

use std::path::{Path, PathBuf};

use rbs_proto::Priority;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use env::{EnvOverrides, env_allowed, filter_env};
pub use layers::{Layer, merge_layers};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid config {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid value for {var}: {message}")]
    Env { var: &'static str, message: String },
    #[error("could not determine home directory")]
    NoHome,
}

/// Where the shim should run a job. `Auto` walks the fallback chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Auto,
    Remote,
    Local,
    Plain,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Mode::Auto),
            "remote" => Ok(Mode::Remote),
            "local" => Ok(Mode::Local),
            "plain" => Ok(Mode::Plain),
            other => Err(format!(
                "unknown mode `{other}` (expected auto|remote|local|plain)"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostBuild {
    PullAndLink,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Remote {
    pub host: String,
    pub enabled: bool,
    pub max_rtt_ms: u64,
    pub connect_timeout_ms: u64,
    /// Empty = identical absolute paths on both hosts.
    pub mirror_root: String,
    /// Path of the `rbs` binary on the remote host, relative to its home dir
    /// unless absolute. Non-interactive ssh shells often lack `~/.local/bin` on PATH.
    pub remote_bin: String,
}

impl Default for Remote {
    fn default() -> Self {
        Self {
            host: "node0".into(),
            enabled: true,
            max_rtt_ms: 40,
            connect_timeout_ms: 3000,
            mirror_root: String::new(),
            remote_bin: ".local/bin/rbs".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LocalServer {
    pub enabled: bool,
    pub socket: PathBuf,
    pub autostart: bool,
}

impl Default for LocalServer {
    fn default() -> Self {
        Self {
            enabled: true,
            socket: PathBuf::from("~/.local/state/rbs/server.sock"),
            autostart: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Policy {
    pub mode: Mode,
    pub local_subcommands: Vec<String>,
    pub post_build: PostBuild,
    pub env_allowlist: Vec<String>,
    pub priority: Priority,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            local_subcommands: vec!["run".into()],
            post_build: PostBuild::PullAndLink,
            env_allowlist: vec![
                "CARGO_*".into(),
                "RUST*".into(),
                "CC".into(),
                "CXX".into(),
                "PKG_CONFIG_*".into(),
                "SQLX_OFFLINE".into(),
                "DATABASE_URL".into(),
            ],
            priority: Priority::Agent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// 0 = `nproc - reserve_cores`.
    pub tokens: u32,
    pub reserve_cores: u32,
    pub max_jobs: u32,
    pub min_mem_available_gib: u32,
    pub job_mem_max_gib: u32,
    pub queue_limit: u32,
    pub job_timeout_secs: u64,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            tokens: 0,
            reserve_cores: 2,
            max_jobs: 8,
            min_mem_available_gib: 8,
            job_mem_max_gib: 24,
            queue_limit: 32,
            job_timeout_secs: 3600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Sync {
    pub rsync_path: String,
    pub extra_excludes: Vec<String>,
    pub force_include: Vec<String>,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            rsync_path: "rsync".into(),
            extra_excludes: Vec::new(),
            force_include: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub remote: Remote,
    pub local_server: LocalServer,
    pub policy: Policy,
    pub server: Server,
    pub sync: Sync,
}

impl Config {
    /// Parse a single TOML document as a complete config (missing sections take defaults).
    pub fn from_toml(path: &Path, text: &str) -> Result<Config, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Expand `~` in path fields; after this every path is absolute.
    pub fn expand_paths(&mut self, home: &Path) {
        self.local_server.socket = expand_tilde(&self.local_server.socket, home);
    }
}

fn expand_tilde(p: &Path, home: &Path) -> PathBuf {
    match p.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// Resolve the user's config file path (`$XDG_CONFIG_HOME/rbs/config.toml` or `~/.config/...`).
pub fn user_config_path(home: &Path, xdg_config_home: Option<&Path>) -> PathBuf {
    xdg_config_home
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".config"))
        .join("rbs/config.toml")
}

/// Find the nearest `.rbs.toml` walking up from `start`.
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|d| d.join(".rbs.toml"))
        .find(|p| p.is_file())
}

/// Load the fully layered configuration for a command running in `cwd`.
pub fn load(cwd: &Path) -> Result<Config, ConfigError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(ConfigError::NoHome)?;
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let mut layers = Vec::new();
    let user_path = match std::env::var_os("RBS_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => user_config_path(&home, xdg.as_deref()),
    };
    if let Some(l) = Layer::read_optional(&user_path)? {
        layers.push(l);
    }
    if let Some(p) = find_project_config(cwd)
        && let Some(l) = Layer::read_optional(&p)?
    {
        layers.push(l);
    }
    let mut cfg = merge_layers(&layers)?;
    EnvOverrides::from_env()?.apply(&mut cfg);
    cfg.expand_paths(&home);
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.policy.mode, Mode::Auto);
        assert_eq!(c.remote.host, "node0");
        assert_eq!(c.policy.local_subcommands, vec!["run"]);
        assert_eq!(c.server.tokens, 0);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let c = Config::from_toml(
            Path::new("t.toml"),
            "[remote]\nhost = \"big\"\nmax_rtt_ms = 5\n",
        )
        .expect("parse");
        assert_eq!(c.remote.host, "big");
        assert_eq!(c.remote.max_rtt_ms, 5);
        assert_eq!(c.remote.remote_bin, ".local/bin/rbs");
        assert_eq!(
            c.remote.connect_timeout_ms,
            Remote::default().connect_timeout_ms
        );
        assert_eq!(c.policy, Policy::default());
    }

    #[test]
    fn unknown_keys_are_errors() {
        let err = Config::from_toml(Path::new("t.toml"), "[policy]\nmod = \"auto\"\n")
            .expect_err("must fail");
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains("t.toml"));
    }

    #[test]
    fn enums_parse_from_snake_case() {
        let c = Config::from_toml(
            Path::new("t"),
            "[policy]\nmode = \"plain\"\npost_build = \"none\"\npriority = \"background\"\n",
        )
        .expect("parse");
        assert_eq!(c.policy.mode, Mode::Plain);
        assert_eq!(c.policy.post_build, PostBuild::None);
        assert_eq!(c.policy.priority, Priority::Background);
        assert!("bogus".parse::<Mode>().is_err());
    }

    #[test]
    fn tilde_expansion_and_paths() {
        let mut c = Config::default();
        c.expand_paths(Path::new("/home/u"));
        assert_eq!(
            c.local_server.socket,
            PathBuf::from("/home/u/.local/state/rbs/server.sock")
        );
        assert_eq!(
            user_config_path(Path::new("/home/u"), None),
            PathBuf::from("/home/u/.config/rbs/config.toml")
        );
        assert_eq!(
            user_config_path(Path::new("/home/u"), Some(Path::new("/x"))),
            PathBuf::from("/x/rbs/config.toml")
        );
    }

    #[test]
    fn finds_nearest_project_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("a/b/c")).expect("mkdir");
        std::fs::write(root.join("a/.rbs.toml"), "").expect("write");
        assert_eq!(
            find_project_config(&root.join("a/b/c")),
            Some(root.join("a/.rbs.toml"))
        );
        assert_eq!(find_project_config(root), None);
    }
}
