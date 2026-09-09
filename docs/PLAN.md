# Implementation plan

## Decisions (locked 2026-08-22)

| Topic | Decision |
|---|---|
| Transport | SSH only. `ssh node0 rbs proxy` bridges stdio ↔ node0's unix socket. No ports, no TLS. |
| Shared store | kache S3 remote → MinIO on node0 (`rbs-minio`, port 9100). |
| Tests | Run on remote by default. Escape hatch: `RBS_LOCAL=1` / `RBS_MODE=local|plain`, or `[policy] local_subcommands` in config. |
| Worktree layout | Mirrored: same absolute path on both hosts (same user + `$HOME`). |
| Agent integration | Transparent: a binary named `cargo` in a PATH dir prepended for agents; real cargo resolved by skipping the shim's own dir. |
| Toolchain | Fingerprint on both sides per workspace; mismatch = hard error with both fingerprints printed. Never silently fall back on mismatch. |
| Fallback | remote → local `rbs server` (same scheduler, laptop-sized pool) → plain cargo + local kache. Each step logged at `warn`. |

## Phases

0. **Experiment (done).** kache keys portable cross-machine; S3 path works; link
   quality matters; demand-GET deadline means explicit pull.
1. **Foundation (main branch, sequential).** Workspace, `rbs-proto`,
   `rbs-config`, `rbs-toolchain` with tests. Types first so workstreams build
   against a stable contract.
2. **Parallel workstreams (worktrees, one PR each).**
   - `feat/server` — `rbs-server`: socket API, jobserver FIFO pool, admission,
     queue, systemd-run scopes, `rbs server` / `rbs proxy` subcommands.
   - `feat/client` — `rbs-client` + `rbs-sync`: cargo shim, backend chain,
     rsync, job streaming, pull-and-link, `rbs status`.
   - `feat/setup` — `rbs setup` / `rbs doctor`, systemd user units, kache
     config generation, `deploy/` docs.
   Expected conflicts: `crates/rbs-client/src/main.rs` subcommand dispatch
   (server/proxy vs setup/doctor) — trivial to merge.
3. **Integration (done 2026-08-22).** Local mode end-to-end on the laptop and
   remote mode against node0 both verified with the probe project (build, test,
   pull-and-link). Three integration findings fixed: ssh PATH → `remote_bin`;
   shim fork-bomb via `cargo -V` → fingerprint guard + passthrough table;
   scoped jobs failing without the user bus → inherited env + honest probe.
   Still to do: multi-agent load test (N concurrent `cargo build` through the shim).

## Testing strategy

- Unit tests in every crate, written before implementation (protocol framing
  round-trips, config layering, fingerprint parsing/compare, backend-selection
  decision table with mocked probes, rsync argv construction, token pool
  accounting, admission decisions).
- Server integration tests spawn `rbs server` on a temp socket and run `sh -c`
  jobs; assert streaming order, exit codes, and concurrency limits.
- No test touches node0 or the network; remote paths are exercised through a
  `Transport` trait with an in-process fake.

## Non-goals (v1)

Multi-node scheduling, per-crate cost prediction, web UI, Windows/macOS,
caching C/C++ via kache cc wrappers, replacing kache's own daemon.
