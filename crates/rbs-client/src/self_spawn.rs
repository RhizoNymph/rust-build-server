//! Build the `Command` for a detached self-spawn (`store-touch` after a
//! build, local-server autostart) that must land in `cli_main`, never back
//! in the `cargo` shim.
//!
//! `invoked_as_cargo()` (main.rs) dispatches purely on `argv[0]`'s file
//! name: literally `cargo` → shim flow, anything else → `rbs` CLI. Both
//! self-spawns exec `self.exe` — the running binary's own path — which, when
//! *this* process was itself invoked as `cargo` through the shim
//! hardlink/symlink at `~/.local/share/rbs/shim/cargo`, IS a path whose file
//! name is `cargo`. Spawning it with the default argv[0] (the program path)
//! makes the child's `invoked_as_cargo()` return `true`, so e.g.
//! `store-touch` gets routed through the shim's job-submission path instead
//! of `Cmd::StoreTouch`, and `server` falls through fingerprinting instead
//! of starting the daemon.
//!
//! Setting argv[0] explicitly to something that is never `cargo` fixes this
//! regardless of what `self.exe`'s file name happens to be.

use std::path::Path;
use std::process::Command;

/// argv[0] used for detached self-spawns. Anything other than `cargo` works;
/// `rbs` is the most readable in `ps`/journal output.
pub const SAFE_ARGV0: &str = "rbs";

/// `Command::new(program)` with argv[0] overridden to [`SAFE_ARGV0`], so the
/// child never re-enters the shim's `invoked_as_cargo()` dispatch no matter
/// what `program`'s file name looks like. Callers add subcommand args and
/// stdio config on the returned `Command`.
pub fn command(program: &Path) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(program);
    cmd.arg0(SAFE_ARGV0);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// `/bin/sh -c 'printf %s "$0"'`: with no `command_name` operand after
    /// `-c`, POSIX `sh` sets `$0` to the name the shell itself was invoked
    /// with — its own argv[0], exactly the value [`command`] sets. `/bin/sh`
    /// is a real standalone binary (unlike a multi-call dispatcher such as
    /// a uutils-coreutils install, which can resolve its own identity some
    /// other way and would not exercise the override), so this reads back
    /// the literal argv[0] the child process received.
    fn spawned_dollar_zero(program: &Path) -> Option<Vec<u8>> {
        if !program.exists() {
            return None;
        }
        let out = command(program)
            .arg("-c")
            .arg("printf %s \"$0\"")
            .output()
            .expect("spawn");
        assert!(out.status.success(), "{:?}", out);
        Some(out.stdout)
    }

    #[test]
    fn argv0_is_overridden_not_the_program_file_name() {
        let sh = PathBuf::from("/bin/sh");
        match spawned_dollar_zero(&sh) {
            Some(argv0) => assert_eq!(argv0, SAFE_ARGV0.as_bytes()),
            None => eprintln!("/bin/sh not available; skipping"),
        }
    }

    /// The exact bug scenario: `program`'s path is literally named `cargo`
    /// (as `self.exe` is when this process was itself invoked through the
    /// shim hardlink/symlink). Without the override, `$0` here would be the
    /// full `.../cargo` path; the override must still win.
    #[test]
    fn argv0_override_wins_even_when_program_path_is_named_cargo() {
        let sh = PathBuf::from("/bin/sh");
        if !sh.exists() {
            eprintln!("/bin/sh not available; skipping");
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let link = tmp.path().join("cargo");
        std::os::unix::fs::symlink(&sh, &link).expect("symlink");
        assert_eq!(
            link.file_name().unwrap(),
            "cargo",
            "test setup: program path must be named cargo"
        );
        let argv0 = spawned_dollar_zero(&link).expect("linked sh exists");
        assert_eq!(argv0, SAFE_ARGV0.as_bytes());
    }
}
