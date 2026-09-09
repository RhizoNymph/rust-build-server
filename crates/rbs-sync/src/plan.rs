//! Pure rsync argv construction. No I/O; unit tested exactly.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Everything needed to mirror a workspace root to the same path on `host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsyncPlan {
    /// rsync binary (`[sync] rsync_path`).
    pub rsync_path: String,
    /// Absolute workspace root on the laptop; mirrored to the same path remotely.
    pub root: PathBuf,
    /// ssh destination.
    pub host: String,
    /// Extra `--exclude` patterns (`[sync] extra_excludes`), after the built-ins.
    pub excludes: Vec<String>,
    /// `--include` patterns that override `.gitignore` (`[sync] force_include`).
    pub force_include: Vec<String>,
}

impl RsyncPlan {
    pub fn new(cfg: &rbs_config::Sync, root: &Path, host: &str) -> Self {
        Self {
            rsync_path: cfg.rsync_path.clone(),
            root: root.to_path_buf(),
            host: host.to_string(),
            excludes: cfg.extra_excludes.clone(),
            force_include: cfg.force_include.clone(),
        }
    }

    /// `<root>/` — trailing slash so rsync copies contents, not the directory itself.
    pub fn source(&self) -> OsString {
        let mut s = self.root_no_slash();
        s.push("/");
        s
    }

    /// Root as bytes with any trailing `/` removed (Path keeps it verbatim).
    fn root_no_slash(&self) -> OsString {
        let s = self.root.to_string_lossy();
        let trimmed = if s.len() > 1 {
            s.trim_end_matches('/')
        } else {
            &s
        };
        OsString::from(trimmed)
    }

    /// `<host>:<root>/`
    pub fn destination(&self) -> OsString {
        let mut s = OsString::from(&self.host);
        s.push(":");
        s.push(self.root_no_slash());
        s.push("/");
        s
    }

    /// Full argv including the program. Order matters for rsync's filter chain:
    /// `--include` entries come first so they beat both the `.gitignore` merge
    /// rule and every `--exclude` that follows.
    pub fn argv(&self) -> Vec<OsString> {
        let mut v: Vec<OsString> = vec![
            self.rsync_path.clone().into(),
            "-a".into(),
            "--delete".into(),
            "--mkpath".into(),
        ];
        for inc in &self.force_include {
            v.push(format!("--include={inc}").into());
        }
        v.push("--filter=:- .gitignore".into());
        v.push("--exclude=.git/".into());
        v.push("--exclude=target/".into());
        for ex in &self.excludes {
            v.push(format!("--exclude={ex}").into());
        }
        v.push(self.source());
        v.push(self.destination());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn default_argv_is_exact() {
        let plan = RsyncPlan::new(
            &rbs_config::Sync::default(),
            Path::new("/home/u/Code/proj"),
            "node0",
        );
        assert_eq!(
            strs(&plan.argv()),
            vec![
                "rsync",
                "-a",
                "--delete",
                "--mkpath",
                "--filter=:- .gitignore",
                "--exclude=.git/",
                "--exclude=target/",
                "/home/u/Code/proj/",
                "node0:/home/u/Code/proj/",
            ]
        );
    }

    #[test]
    fn includes_precede_filter_and_excludes_follow_builtins() {
        let cfg = rbs_config::Sync {
            rsync_path: "/usr/bin/rsync".into(),
            extra_excludes: vec!["*.log".into(), "node_modules/".into()],
            force_include: vec![".env".into(), "secrets/".into()],
        };
        let plan = RsyncPlan::new(&cfg, Path::new("/w"), "big");
        assert_eq!(
            strs(&plan.argv()),
            vec![
                "/usr/bin/rsync",
                "-a",
                "--delete",
                "--mkpath",
                "--include=.env",
                "--include=secrets/",
                "--filter=:- .gitignore",
                "--exclude=.git/",
                "--exclude=target/",
                "--exclude=*.log",
                "--exclude=node_modules/",
                "/w/",
                "big:/w/",
            ]
        );
    }

    #[test]
    fn trailing_slash_is_not_doubled_but_always_present() {
        // Path normalises away a trailing slash; the plan must re-add exactly one.
        let plan = RsyncPlan::new(&rbs_config::Sync::default(), Path::new("/w/x/"), "h");
        assert_eq!(plan.source(), OsString::from("/w/x/"));
        assert_eq!(plan.destination(), OsString::from("h:/w/x/"));
    }
}
