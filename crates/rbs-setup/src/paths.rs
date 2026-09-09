//! Filesystem locations used by setup/doctor, derived from the environment
//! once so tests can substitute temp directories.

use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathsError {
    #[error("could not determine home directory ($HOME unset)")]
    NoHome,
    #[error("could not determine the running executable: {0}")]
    NoSelfExe(#[source] std::io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    pub config_dir: PathBuf,
    /// The running `rbs` binary.
    pub self_exe: PathBuf,
    /// `$PATH` split into directories, in order.
    pub path_dirs: Vec<PathBuf>,
}

impl Paths {
    pub fn from_env(self_exe: Option<PathBuf>) -> Result<Self, PathsError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(PathsError::NoHome)?;
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let self_exe = match self_exe {
            Some(p) => p,
            None => std::env::current_exe().map_err(PathsError::NoSelfExe)?,
        };
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Ok(Self {
            home,
            config_dir,
            self_exe,
            path_dirs,
        })
    }

    pub fn rbs_config(&self) -> PathBuf {
        rbs_config::user_config_path(&self.home, Some(&self.config_dir))
    }

    pub fn minio_env(&self) -> PathBuf {
        self.config_dir.join("rbs/minio.env")
    }

    pub fn kache_config(&self) -> PathBuf {
        self.config_dir.join("kache/config.toml")
    }

    pub fn aws_credentials(&self) -> PathBuf {
        self.home.join(".aws/credentials")
    }

    pub fn systemd_user_dir(&self) -> PathBuf {
        self.config_dir.join("systemd/user")
    }

    pub fn shim_dir(&self) -> PathBuf {
        self.home.join(".local/share/rbs/shim")
    }

    /// `~/.local/bin/kache` when present (the documented install location), else `kache` from PATH.
    pub fn kache_bin(&self) -> String {
        let p = self.home.join(".local/bin/kache");
        if p.is_file() {
            p.display().to_string()
        } else {
            "kache".to_string()
        }
    }

    /// First directory on PATH containing an executable named `name`.
    pub fn find_on_path(&self, name: &str) -> Option<PathBuf> {
        self.path_dirs
            .iter()
            .map(|d| d.join(name))
            .find(|p| is_executable(p))
    }
}

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
