# toolchain — fingerprint & mismatch detection

## Scope
Compute a `ToolchainFingerprint` for a workspace directory on the current host
and compare two fingerprints, producing a loud, structured error. Non-scope:
installing toolchains or reconciling a mismatch — that is
`rbs doctor --sync-toolchain` (`docs/features/toolchain-sync.md`) and
`rbs bootstrap`'s toolchain step.

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

### Explicit `cargo +<toolchain> …` overrides
`cargo`'s own `+toolchain` syntax (`cargo +stable build`, `cargo
+nightly-2026-10-02 install …`) names a toolchain directly on the command
line, overriding whatever the directory would otherwise resolve to. The shim
parses this token out of `argv` (see `docs/features/client.md`'s "Subcommand
detection") and threads it through fingerprinting on *both* sides so the
mismatch check compares the toolchain the command actually asked for, not the
directory's default:
- `fingerprint(cwd, toolchain_override)` — when `toolchain_override` is
  `Some(name)`, steps 1–2 become `rustc +<name> -vV` / `cargo +<name> -V`
  (the rustup proxies resolve `+<name>` exactly as they would for a real
  `cargo +<name> …` invocation), and step 3 is skipped: `toolchain_name` is
  set directly to `name` instead of querying the directory's default via
  `rustup show active-toolchain`, which would report the wrong toolchain here.
- The client sends the override alongside the client fingerprint on
  `JobRequest.toolchain_override`; the server's `Runner::check_toolchain`
  (`crates/rbs-server/src/runner.rs`) fingerprints its own mirrored `cwd` with
  that same override before comparing, so both sides resolve `+<name>` to
  concrete installs independently and a drift between them is still caught.
- A mismatch under an explicit override is **always** a hard error (same as
  a `rust-toolchain.toml` pin), even in a workspace with no pin file — see
  `ShimInput::hard_mismatch` in `docs/features/client.md`. The override is
  itself a binding, explicit contract; falling back silently to a different
  toolchain would violate exactly what the user asked for.

## Files
- `crates/rbs-toolchain/src/lib.rs` — `fingerprint(cwd, toolchain_override: Option<&str>) -> Result<ToolchainFingerprint, ToolchainError>`, `check(client, server) -> Result<(), Mismatch>`, `parse_rustc_vv(&str)`.

## Invariants
- Both the shim and `rbs doctor`'s `remote-toolchain` check use the same contract-aware rule, so they never disagree about whether a mismatch is a problem.
- `find_toolchain_file(cwd)` walks ancestors for `rust-toolchain.toml`/`rust-toolchain` (nearest wins) — the client uses it to decide whether a mismatch is a broken contract (hard error) or an unpinned workspace (warn + fall back). An explicit `+toolchain` override on argv is a second, independent source of the same hard-error rule (see above); the two are OR'd together, not merged into one field, so each keeps a single meaning.
- Every subprocess spawned by `fingerprint` carries `RBS_SHIM_ACTIVE=1` (`SHIM_GUARD_ENV`) so the `cargo` shim execs the real cargo instead of recursing.
- Equality = `rustc_commit` && `host` (cargo version/toolchain name are informational; they are printed but do not decide).
- Nightly without a pinned date is flagged in the hint text (commit hash will drift daily); this applies equally to a nightly named via `+nightly` without a date.
- `rustc`/`cargo` are only ever invoked with a `+<name>` prefix when an explicit override is given; without one, fingerprinting is unchanged from before this override support existed.
