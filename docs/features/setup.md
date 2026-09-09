# setup — install & verify

## Scope
One-command provisioning of a host (laptop or node0) and a health check.
Non-scope: installing rustup/toolchains, running MinIO (documented in
`deploy/minio.md`), the `rbs` CLI argument parsing (lives in `rbs-client`).

## Commands
- `rbs setup [--role laptop|node0] [--remote-host node0] [--force]` → `rbs_setup::setup(SetupOpts)`
  1. Write `~/.config/rbs/config.toml` if absent. Generated from
     `rbs_config::Config::default()` via `toml::to_string_pretty` with
     role-specific `[server]` sizing — node0: `reserve_cores=4, max_jobs=12,
     job_mem_max_gib=24, min_mem_available_gib=16` plus `[remote] enabled=false`;
     laptop: `reserve_cores=4, max_jobs=3, job_mem_max_gib=12,
     min_mem_available_gib=8`. If the file exists and differs, a line diff is
     printed and the file is kept (setup continues) unless `--force`.
  2. Read `~/.config/rbs/minio.env` (`KACHE_S3_ACCESS_KEY`, `KACHE_S3_SECRET_KEY`
     required; `KACHE_S3_ENDPOINT`, `KACHE_S3_BUCKET` optional overrides of
     `http://127.0.0.1:9100` / `kache`). Write `~/.config/kache/config.toml`
     (same diff/force policy) with exactly:
     ```toml
     [cache.remote]
     type = "s3"
     bucket = "kache"
     endpoint = "http://127.0.0.1:9100"
     region = "us-east-1"
     profile = "rbs"
     prefix = "artifacts"
     ```
     Ensure the `[rbs]` profile in `~/.aws/credentials` (mode 600, `~/.aws`
     mode 700; only the `[rbs]` section is replaced/appended, other profiles are
     byte-for-byte preserved). Validate with `kache stats`: a line starting
     `Remote:     s3://` must be present (kache silently ignores malformed
     TOML), else `SetupError::KacheRemoteNotConfigured`.
  3. Write `~/.config/systemd/user/rbs-server.service` from
     `deploy/systemd/rbs-server.service` (`include_str!`, `{self_exe}`
     substituted), `systemctl --user daemon-reload`, `systemctl --user enable
     --now rbs-server`, then `kache daemon install` (a failure mentioning
     "already" is ignored).
  4. `~/.local/share/rbs/shim/cargo` → hardlink to `self_exe`, symlink
     fallback; an existing link is replaced. Prints
     `export PATH="$HOME/.local/share/rbs/shim:$PATH"`.
  5. Laptop role with `--remote-host H`: `ssh H mkdir -p .local/bin`,
     `scp <self_exe> H:.local/bin/rbs`, `ssh H .local/bin/rbs setup --role node0
     [--force]`; the remote's stdout is echoed. Node0 role ignores
     `--remote-host` with a warning (no recursion).
- `rbs doctor [--remote]` → `rbs_setup::doctor(DoctorOpts{cwd, remote}) -> DoctorReport`.
  Each check is a `Check{name, ok, detail}`; `DoctorReport::ok()` is the AND.
  A command that fails or cannot be spawned is a failed check with its stderr
  in `detail` — never a panic or an `Err`.

  | name | ok when |
  |---|---|
  | `binary:<rbs\|cargo\|rustup\|kache\|rsync\|ssh\|systemd-run>` | executable found in `$PATH` (detail = path) |
  | `kache-remote` | `kache stats` prints `Remote:     s3://…` |
  | `kache-daemon` | `kache daemon status` contains "running" (and not "not running") |
  | `server-socket` | `cfg.local_server.socket` exists |
  | `shim-precedence` | first `cargo` on `$PATH` is under `~/.local/share/rbs/shim` (detail carries the PATH line otherwise) |
  | `toolchain` | `rustc -vV` / `cargo -V` / `rustup show active-toolchain` in `cwd` parse into a fingerprint (detail = fingerprint) |
  | `remote-ssh` (`--remote`) | `ssh -o BatchMode=yes -o ConnectTimeout=3 <host> true` succeeds; detail has the RTT in ms. Failure marks `remote-rbs`/`remote-toolchain` as skipped+failed. |
  | `remote-rbs` | `ssh <host> <remote_bin> --version` succeeds (detail = version); `remote_bin` from `[remote]` (default `.local/bin/rbs`) |
  | `remote-toolchain` | `ssh <host> 'cd <cwd> && rustc -vV'` parsed with `rbs_toolchain::parse_rustc_vv` is `compatible_with` the local fingerprint; on mismatch detail lists both fingerprints and the `rustup toolchain install` hint |

  `<host>` is `cfg.remote.host` from the layered rbs config for `cwd`.

## Data / control flow
```
setup(opts)  ──▶ Paths::from_env(self_exe) ─┐
                                             ├─▶ setup_with(&SystemRunner, &paths, &opts) -> Result<(), SetupError>
doctor(opts) ──▶ Paths::from_env(None)       │        steps 1..5 above, each logged via tracing info!/warn! with kv fields
                 rbs_config::load(cwd) ──────┴─▶ doctor_with(&SystemRunner, &paths, &cfg, &opts) -> Result<DoctorReport, RunnerError>
```
Every external process goes through `Runner::run_in(cwd, program, args) ->
Result<Output, RunnerError>` (`run` = no cwd). Every filesystem location comes
from `Paths { home, config_dir, self_exe, path_dirs }`. Tests call the
`*_with` functions with a scripted `FakeRunner` and a tempdir `Paths`, so no
test spawns a process, reads the real `$HOME`, or touches the network.

## Files
- `crates/rbs-setup/src/lib.rs` — public API: `Role`, `SetupOpts`, `DoctorOpts`,
  `Check`, `DoctorReport`, `setup`, `doctor`; re-exports `setup_with`,
  `doctor_with`, `Paths`, `Runner`, `SystemRunner`, `Output`, `RunnerError`,
  `SetupError`, `generate_config`, `render_unit`, `REQUIRED_BINARIES`,
  `KACHE_CONFIG_DEFAULT`.
- `crates/rbs-setup/src/runner.rs` — `Runner` trait, `Output{status, stdout, stderr}`, `SystemRunner`.
- `crates/rbs-setup/src/paths.rs` — `Paths` (+ `rbs_config()`, `minio_env()`,
  `kache_config()`, `aws_credentials()`, `systemd_user_dir()`, `shim_dir()`,
  `kache_bin()`, `find_on_path()`).
- `crates/rbs-setup/src/setup.rs` — steps 1–5, `SetupError`, `generate_config`,
  `kache_config`, `render_unit`, `line_diff`, `aws_credentials_with_profile`.
- `crates/rbs-setup/src/doctor.rs` — check table above.
- `crates/rbs-setup/src/testing.rs` (cfg(test)) — `FakeRunner`, `TempHome`.
- `crates/rbs-setup/src/{setup_tests,doctor_tests}.rs` — 29 tests.
- `deploy/systemd/rbs-server.service` — unit template (`ExecStart={self_exe} server`,
  `Restart=on-failure`, `Environment=RBS_LOG=info`, `WantedBy=default.target`).
- `deploy/minio.md` — the `rbs-minio` container and bucket; `deploy/README.md` —
  laptop + node0 bring-up, toolchain pinning, PATH line for agents.

## Invariants and constraints
- Never overwrite an existing config file (`rbs` or `kache`) without `--force`;
  print a diff instead and keep going. Generated files (systemd unit, shim) are
  always rewritten.
- Secrets are read only from `~/.config/rbs/minio.env`, written only to
  `~/.aws/credentials` (0600), never into TOML, never logged or included in
  error messages.
- `kache` is resolved as `~/.local/bin/kache` when that file exists, else
  `kache` from PATH.
- The kache endpoint/bucket are not part of the rbs config schema; override
  them via `KACHE_S3_ENDPOINT` / `KACHE_S3_BUCKET` in `minio.env`.
- `doctor` exit code is non-zero if any check fails (agents can gate on it);
  the binary decides that from `DoctorReport::ok()`.
- `doctor` fingerprints the local toolchain through the `Runner` (same logic as
  `rbs_toolchain::fingerprint`) so it stays hermetic; the remote fingerprint
  only carries `rustc_commit`/`rustc_version`/`host`, which is all
  `compatible_with` compares.
