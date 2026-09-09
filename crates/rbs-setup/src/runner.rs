//! Injectable command execution so setup/doctor are testable without a shell.

use std::path::Path;
use std::process::Command;

use thiserror::Error;

/// Captured result of a finished process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Exit code; `None` when killed by a signal.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(stdout: &str) -> Self {
        Self {
            status: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// Human-readable failure summary: stderr if any, else stdout, else the status.
    pub fn failure_detail(&self) -> String {
        let err = self.stderr.trim();
        if !err.is_empty() {
            return err.to_string();
        }
        let out = self.stdout.trim();
        if !out.is_empty() {
            return out.to_string();
        }
        match self.status {
            Some(c) => format!("exit code {c}"),
            None => "killed by signal".to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("failed to spawn `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

/// Runs external programs. Production uses [`SystemRunner`]; tests script a fake.
pub trait Runner {
    /// Run `program args…` with an optional working directory.
    fn run_in(
        &self,
        cwd: Option<&Path>,
        program: &str,
        args: &[&str],
    ) -> Result<Output, RunnerError>;

    fn run(&self, program: &str, args: &[&str]) -> Result<Output, RunnerError> {
        self.run_in(None, program, args)
    }
}

/// Real process execution via [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run_in(
        &self,
        cwd: Option<&Path>,
        program: &str,
        args: &[&str],
    ) -> Result<Output, RunnerError> {
        let mut cmd = Command::new(program);
        cmd.args(args);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        let out = cmd.output().map_err(|source| RunnerError::Spawn {
            program: program.to_string(),
            source,
        })?;
        Ok(Output {
            status: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}
