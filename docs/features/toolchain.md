# toolchain — fingerprint & mismatch detection

## Scope
Compute a `ToolchainFingerprint` for a workspace directory on the current host
and compare two fingerprints, producing a loud, structured error. Non-scope:
installing toolchains.

## Flow
1. In `cwd`, run `rustc -vV` (via rustup proxy so `rust-toolchain.toml` is honoured) → parse `release`, `commit-hash`, `host`.
2. Run `cargo -V` → version string.
3. Run `rustup show active-toolchain` → toolchain name (best-effort; empty if rustup absent).
4. Server side: same function in the mirrored cwd after rsync; compare with request's fingerprint; mismatch → `JobEvent::Rejected(ToolchainMismatch{..})`.
5. Client renders mismatch as:
   ```
   error: toolchain mismatch between laptop and node0 — refusing to build
     laptop: rustc 1.95.0 (59807616e) x86_64-unknown-linux-gnu [1.95.0-x86_64-unknown-linux-gnu]
     node0:  rustc 1.96.0 (ac68faa20) x86_64-unknown-linux-gnu [stable-x86_64-unknown-linux-gnu]
     hint: pin the project with rust-toolchain.toml and run `rustup toolchain install 1.95.0` on node0
   ```
   and exits 1. No fallback.

## Files
- `crates/rbs-toolchain/src/lib.rs` — `fingerprint(cwd) -> Result<ToolchainFingerprint, ToolchainError>`, `check(client, server) -> Result<(), Mismatch>`, `parse_rustc_vv(&str)`.

## Invariants
- Both the shim and `rbs doctor`'s `remote-toolchain` check use the same contract-aware rule, so they never disagree about whether a mismatch is a problem.
- `find_toolchain_file(cwd)` walks ancestors for `rust-toolchain.toml`/`rust-toolchain` (nearest wins) — the client uses it to decide whether a mismatch is a broken contract (hard error) or an unpinned workspace (warn + fall back).
- Every subprocess spawned by `fingerprint` carries `RBS_SHIM_ACTIVE=1` (`SHIM_GUARD_ENV`) so the `cargo` shim execs the real cargo instead of recursing.
- Equality = `rustc_commit` && `host` (cargo version/toolchain name are informational; they are printed but do not decide).
- Nightly without a pinned date is flagged in the hint text (commit hash will drift daily).
