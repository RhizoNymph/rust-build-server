//! Test doubles: a scripted [`Runner`] and a temp-dir [`Paths`].

use std::cell::RefCell;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::paths::Paths;
use crate::runner::{Output, Runner, RunnerError};

/// One scripted response. `args == None` matches any argument list.
#[derive(Debug, Clone)]
pub struct Script {
    pub program: String,
    pub args: Option<Vec<String>>,
    pub output: Output,
}

/// One recorded invocation: (cwd, program, args).
pub type Call = (Option<PathBuf>, String, Vec<String>);

/// A [`Runner`] that answers from a script and records every call.
///
/// Lookup prefers an entry with matching args, then a program-wide wildcard;
/// later entries override earlier ones.
/// Programs with no entry at all behave like a missing binary (spawn error).
#[derive(Debug, Default)]
pub struct FakeRunner {
    scripts: Vec<Script>,
    pub calls: RefCell<Vec<Call>>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Any invocation of `program` succeeds with `stdout`.
    pub fn ok(mut self, program: &str, stdout: &str) -> Self {
        self.scripts.push(Script {
            program: program.into(),
            args: None,
            output: Output::ok(stdout),
        });
        self
    }

    /// `program args…` succeeds with `stdout`.
    pub fn ok_args(mut self, program: &str, args: &[&str], stdout: &str) -> Self {
        self.scripts.push(Script {
            program: program.into(),
            args: Some(args.iter().map(|s| s.to_string()).collect()),
            output: Output::ok(stdout),
        });
        self
    }

    /// `program args…` exits `status` with `stderr`.
    pub fn fail_args(mut self, program: &str, args: &[&str], status: i32, stderr: &str) -> Self {
        self.scripts.push(Script {
            program: program.into(),
            args: Some(args.iter().map(|s| s.to_string()).collect()),
            output: Output {
                status: Some(status),
                stdout: String::new(),
                stderr: stderr.into(),
            },
        });
        self
    }

    /// Any invocation of `program` exits `status` with `stderr`.
    pub fn fail(mut self, program: &str, status: i32, stderr: &str) -> Self {
        self.scripts.push(Script {
            program: program.into(),
            args: None,
            output: Output {
                status: Some(status),
                stdout: String::new(),
                stderr: stderr.into(),
            },
        });
        self
    }

    pub fn calls_to(&self, program: &str) -> Vec<Vec<String>> {
        self.calls
            .borrow()
            .iter()
            .filter(|(_, p, _)| p == program)
            .map(|(_, _, a)| a.clone())
            .collect()
    }

    pub fn called_with(&self, program: &str, args: &[&str]) -> bool {
        self.calls_to(program).iter().any(|a| a == args)
    }
}

impl Runner for FakeRunner {
    fn run_in(
        &self,
        cwd: Option<&Path>,
        program: &str,
        args: &[&str],
    ) -> Result<Output, RunnerError> {
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.borrow_mut().push((
            cwd.map(Path::to_path_buf),
            program.to_string(),
            argv.clone(),
        ));
        let exact = self
            .scripts
            .iter()
            .rev()
            .find(|s| s.program == program && s.args.as_ref() == Some(&argv));
        let wildcard = self
            .scripts
            .iter()
            .rev()
            .find(|s| s.program == program && s.args.is_none());
        match exact.or(wildcard) {
            Some(s) => Ok(s.output.clone()),
            None => Err(RunnerError::Spawn {
                program: program.to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("fake runner: no script for `{program}`"),
                ),
            }),
        }
    }
}

/// A fresh home directory under a tempdir, with a fake `rbs` binary as `self_exe`.
pub struct TempHome {
    pub dir: tempfile::TempDir,
    pub paths: Paths,
}

impl TempHome {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("bin")).expect("mkdir");
        let self_exe = home.join("bin/rbs");
        std::fs::write(&self_exe, "#!/bin/sh\necho rbs\n").expect("write rbs");
        make_executable(&self_exe);
        let paths = Paths {
            config_dir: home.join(".config"),
            path_dirs: vec![home.join("bin")],
            home,
            self_exe,
        };
        Self { dir, paths }
    }

    /// Write `~/.config/rbs/minio.env` with the given credentials.
    pub fn write_minio_env(&self, access: &str, secret: &str) {
        let p = self.paths.minio_env();
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &p,
            format!(
                "KACHE_S3_ACCESS_KEY={access}\nKACHE_S3_SECRET_KEY={secret}\nKACHE_S3_ENDPOINT=http://127.0.0.1:9100\nKACHE_S3_BUCKET=kache\n"
            ),
        )
        .expect("write minio.env");
    }

    /// Create an executable file named `name` in `dir` and return the dir.
    pub fn fake_bin(&self, dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).expect("mkdir");
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").expect("write");
        make_executable(&p);
        dir.to_path_buf()
    }
}

pub fn make_executable(p: &Path) {
    let mut perm = std::fs::metadata(p).expect("meta").permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm).expect("chmod");
}

pub fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

pub fn mode_of(p: &Path) -> u32 {
    std::fs::metadata(p).expect("meta").permissions().mode() & 0o777
}
