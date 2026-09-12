# rbs — Rust Build Server

```yaml
Overview:
  description: >
    rbs makes `cargo` invocations from many parallel coding agents on a laptop
    run on a big remote box (node0) transparently, with resource-aware
    scheduling, and uses kache (content-addressed build cache with an S3
    remote) as the artifact transport so the laptop never rsyncs `target/`
    back. When the remote is unreachable, saturated, or the link is poor, it
    falls back to a local resource-balanced server, and finally to plain cargo.
  subsystems:
    client: >
      The `cargo` PATH shim + `rbs` CLI. Chooses a backend (remote → local
      server → plain cargo), fingerprints the toolchain, syncs the worktree,
      streams job output, and runs the post-build pull-and-link step.
    server: >
      `rbs server` daemon (same binary on node0 and laptop). Unix-socket job
      API, GNU jobserver token pool shared by every cargo it spawns, memory /
      load admission control and queueing, per-job systemd cgroup scopes.
    proto: Wire types (serde+JSON, newline-delimited) shared by client/server.
    config: Layered config (defaults < ~/.config/rbs/config.toml < .rbs.toml < env).
    toolchain: Toolchain fingerprint (rustc commit hash, host, cargo, rustup name).
    sync: rsync worktree mirroring + kache sync --pull after remote jobs.
    setup: >
      `rbs setup` installs systemd user units (server on both hosts, kache
      daemon), writes kache config for the shared S3 store, and verifies
      everything with `rbs doctor`.
  data_flow: |
    agent ── exec `cargo <args>` ──▶ cargo shim (rbs-client)
      ├─ load Config; honor escape hatches (RBS_MODE=local|plain, RBS_LOCAL=1)
      ├─ passthrough: no subcommand / leading flag / metadata-like subcommands → real cargo
      ├─ fingerprint local toolchain for the workspace (rbs-toolchain)
      ├─ select backend (client::backend):
      │    remote: `ssh node0 .local/bin/rbs proxy` → bridges stdio to node0's unix
      │            socket; probe = Hello/Status round trip (gives RTT +
      │            capacity). Reject if RTT > max_rtt_ms, server saturated,
      │            ssh fails, or toolchain mismatch (pinned workspace → hard error,
      │            no fallback; unpinned → loud warning, next backend).
      │    local:  connect ~/.local/state/rbs/server.sock; autostart if absent.
      │    plain:  exec real cargo with RUSTC_WRAPPER=kache.
      ├─ remote only: rsync worktree → node0 (same absolute path), gitignore
      │    filtered, target/ excluded (rbs-sync)
      ├─ send JobRequest {cwd, argv, env allowlist, toolchain fp, priority}
      │    ◀── stream JobEvent::{Queued, Started, Stdout, Stderr, Exited}
      ├─ server: admission (tokens, MemAvailable, queue) → systemd-run scope →
      │    spawn cargo with MAKEFLAGS=--jobserver-auth=fifo:<pool> and
      │    RUSTC_WRAPPER=kache; kache daemon uploads artifacts to S3 async
      ├─ remote + subcommand in {build} and post_build=pull_and_link:
      │    `kache sync --pull` then local `cargo build` (all hits → link only)
      │    so target/ exists on the laptop for the agent's next step.
      └─ successful compiling build (any backend): detached `rbs store-touch`
           refreshes S3 last-modified on this workspace's store objects
           (throttled), so the store GC sees them as in use.

Features Index:
  proto:
    description: Shared protocol & domain types, framing, versioning.
    entry_points: [crates/rbs-proto/src/lib.rs]
    depends_on: []
    doc: docs/features/proto.md
  config:
    description: Layered configuration, escape hatches, env allowlist.
    entry_points: [crates/rbs-config/src/lib.rs]
    depends_on: [proto]
    doc: docs/features/config.md
  toolchain:
    description: Toolchain fingerprinting and loud mismatch errors.
    entry_points: [crates/rbs-toolchain/src/lib.rs]
    depends_on: [proto]
    doc: docs/features/toolchain.md
  sync:
    description: Worktree mirroring via rsync and kache pull.
    entry_points: [crates/rbs-sync/src/lib.rs]
    depends_on: [config]
    doc: docs/features/sync.md
  server:
    description: Resource-aware job server (jobserver pool, admission, cgroups).
    entry_points: [crates/rbs-server/src/lib.rs, "rbs server"]
    depends_on: [proto, config, toolchain]
    doc: docs/features/server.md
  client:
    description: cargo shim, backend selection & fallback chain, rbs CLI.
    entry_points: [crates/rbs-client/src/main.rs, "cargo (shim)", "rbs"]
    depends_on: [proto, config, toolchain, sync, server]
    doc: docs/features/client.md
  setup:
    description: Install/verify on laptop and node0 (systemd units, kache cfg, doctor).
    entry_points: ["rbs setup", "rbs doctor", deploy/]
    depends_on: [config, client]
    doc: docs/features/setup.md
  store_gc:
    description: >
      Size cap + LFU eviction for the shared kache S3 store (daily timer on
      node0), plus `rbs store-touch`: active workspaces refresh their objects'
      S3 last-modified after builds so eviction recency is store-wide, not
      just node0's index.
    entry_points: [crates/rbs-store/src/lib.rs, "rbs store-gc", "rbs store-touch"]
    depends_on: [config, sync, setup]
    doc: docs/features/store-gc.md
```

## Environment facts the design relies on

- Laptop: 16 cores / 60 GB, glibc 2.43. node0: 32 cores / 125 GB, Ubuntu 24.04,
  glibc 2.39, `ssh node0`, same username and `$HOME` on both (mirrored paths).
- kache 0.14.2 on both at `~/.local/bin/kache`; S3 remote = MinIO on node0
  (endpoint from `~/.config/rbs/minio.env`, bucket `kache`, AWS profile `rbs`).
- Verified: Rust cache keys are identical across the two hosts for mirrored
  paths and the same toolchain. Keys include the rustc commit hash (hence the
  toolchain check) and the glibc version for bin/proc-macro outputs (hence the
  final link always runs locally).
- kache's on-demand remote GET has a 3 s deadline and a circuit breaker; the
  daemon snapshots remote keys at start. Therefore the client does an explicit
  `kache sync --pull` after remote builds instead of relying on demand fetch.
- The laptop is frequently on a high-latency WAN link to node0 (≈120 ms RTT,
  <1 MB/s). Backend selection must measure the link, not assume a LAN.

See `docs/PLAN.md` for the implementation plan and workstreams.
