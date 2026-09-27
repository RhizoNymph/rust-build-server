# toolchain-sync — `rbs doctor --sync-toolchain[=newest|oldest]`

## Scope
Converge this host and the configured remote (`[remote] host`) on one Rust
toolchain for the workspace at `cwd`, when their fingerprints are not
compatible. The target is the **newest** of the two (default) or the
**oldest** (`--sync-toolchain=oldest`). The chosen channel is installed where
missing and pinned in the workspace's toolchain file, so the build-time
contract check then enforces it.

Non-scope: more than two hosts (fleet-wide channel provisioning is
`rbs bootstrap`'s toolchain step); changing any host's `rustup default`;
syncing across different host triples; running at build time (the shim still
refuses a mismatch loudly — see `docs/features/toolchain.md`).

## Flow
```
rbs doctor --sync-toolchain[=P]      (implies --remote; P defaults to newest)
  doctor_with
    local_fingerprint(cwd)  → Probe{fp, commit_date}   (rustc -vV, cargo -V, rustup show active-toolchain)
    remote_checks
      ssh reachable? no  → toolchain-sync FAIL "skipped: ssh unreachable"
      remote_fingerprint  → Probe (ssh host 'cd <cwd> && rustc -vV')
      toolchain_sync::sync(runner, host, cwd, P, local, remote)
        1. compatible (commit + triple)?          → Outcome::AlreadyInSync, nothing touched
        2. release::choose(P)                     → Target{winner, release}
             refuse: different triples (HostTripleMismatch), equal order with
             different commits (Undecidable), unknown release shape
        3. release::pin_channel                   → channel string
             stable        → exact "X.Y.Z" (the winner's rustup name is ignored)
             beta/nightly  → winner's dated rustup name ("nightly-YYYY-MM-DD");
                             remote name via login shell `cd <cwd> && rustup show active-toolchain`;
                             floating names refused (FloatingChannel)
        4. ensure_installed on local, then remote (login shell):
             `rustup toolchain list` → `rustup toolchain install <ch> --profile minimal` if absent
        5. verify: `rustc +<ch> -vV` on both → must be compatible (else Diverged)
        6. pin::write_pin(cwd, ch)                → nearest rust-toolchain(.toml), else creates <cwd>/rust-toolchain.toml
        7. push_pin: `ssh host test -d <dir>` → `scp -q <pin> host:<pin>`; no mirror yet → skipped
                     (the next remote build's rsync carries the pin)
      report:
        Synced   → toolchain-sync ok (what/where/pin), remote-toolchain ok "in sync after --sync-toolchain"
        InSync   → toolchain-sync ok "already in sync", then the normal remote-toolchain check
        Err(e)   → toolchain-sync FAIL e, then the normal contract-aware remote-toolchain check
```

Ordering of releases (`release::Release`, derived `Ord` over fields):
version `(major, minor, patch)` numerically, then channel
`nightly < beta < stable`, then `commit-date`.

## Files
- `crates/rbs-setup/src/toolchain_sync/mod.rs` — `sync(runner, host, cwd, policy, &Probe, &Probe) -> Result<Outcome, SyncError>`,
  `Probe`, `Outcome::{AlreadyInSync, Synced(Box<Synced>)}`, `Synced::describe`, `SyncError` (thiserror),
  install/verify/push helpers over the injectable `Runner`.
- `crates/rbs-setup/src/toolchain_sync/release.rs` — `SyncPolicy` (`FromStr` newest|oldest, default newest),
  `Channel`, `Release::parse`, `commit_date(rustc_vv)`, `choose`, `pin_channel`, `Winner`, `Target`.
- `crates/rbs-setup/src/toolchain_sync/pin.rs` — `write_pin(cwd, channel) -> PathBuf`, `set_channel(text, channel)`.
- `crates/rbs-setup/src/toolchain_sync/{release_tests,pin_tests}.rs` — unit tests; end-to-end doctor
  scenarios live at the bottom of `crates/rbs-setup/src/doctor_tests.rs`.
- `crates/rbs-setup/src/doctor.rs` — `toolchain-sync` check wiring; `local_fingerprint`/`remote_fingerprint` return `Probe`.
- `crates/rbs-setup/src/lib.rs` — `DoctorOpts::sync_toolchain: Option<SyncPolicy>`; re-exports `SyncPolicy`, `SyncError`.
- `crates/rbs-client/src/main.rs` — `--sync-toolchain[=newest|oldest]` (`require_equals`, `default_missing_value = "newest"`).

## Invariants
- Nothing is written unless both hosts have the channel installed **and** `rustc +<ch> -vV` agrees on
  commit + triple. A failed sync never leaves a pin that one host cannot satisfy.
- The pin edit touches only `channel` inside `[toolchain]`; every other line is kept. The result is
  re-parsed and refused (`PinEdit`) if it does not read back as the chosen channel (e.g. a dotted
  `toolchain.channel` key). A legacy one-line `rust-toolchain` file is rewritten as one line.
- Stable is pinned to the exact release, never `stable`, so the pin cannot drift.
- Pre-release channels are only synced from a dated rustup name; an undated `nightly`/`beta` fails
  with a hint to pin `nightly-YYYY-MM-DD`.
- Remote rustup commands go through `ssh host bash -lc '<cmd>'` (`login_shell_args`) so `~/.cargo/bin`
  is on PATH; channel strings are validated shapes (digits/dots or `<name>-YYYY-MM-DD`), so they are
  safe inside that command.
- Syncing may change the rustc commit hash for the workspace, which invalidates its kache entries
  (cache keys include the commit) — a deliberate, operator-requested rebuild.
