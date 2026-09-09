# rbs — Rust Build Server

Transparent, resource-aware remote builds for `cargo`, designed for running
many parallel coding agents on one laptop without melting it.

`rbs` installs a `cargo` PATH shim. When an agent (or you) runs `cargo build`
or `cargo test`, the shim picks a backend:

1. **remote** — ship the invocation to a big box over SSH and run it there,
2. **local** — a resource-balanced job server on this machine,
3. **plain** — plain cargo,

falling through automatically when the remote is unreachable, saturated, or
the link is too slow. Artifacts come back through [kache], a content-addressed
build cache with an S3 remote — `target/` is never rsynced. After a remote
build, the client pulls the cache and relinks locally, so `target/` on your
machine is populated by cache hits plus one link step.

## How resource-awareness works

- Every server owns a **GNU jobserver FIFO** sized to its cores; every cargo it
  spawns inherits it via `MAKEFLAGS`, so N concurrent builds share one pool of
  compile slots instead of each spawning `nproc` rustcs.
- Admission control queues jobs by priority when the box is at `max_jobs` or
  low on memory; each job runs in a systemd scope with `MemoryMax` and
  `CPUWeight`.
- The same server binary runs on the remote box (big pool) and the laptop
  (small pool, the fallback).

## Safety properties

- **Toolchains must match exactly** (rustc commit hash + host triple); a
  mismatch is a loud error with both fingerprints and a `rustup` hint — never a
  silent local rebuild. Pin projects with `rust-toolchain.toml`.
- Non-build invocations (`cargo -V`, `metadata`, `fmt`, …) always pass through
  to real cargo; the shim cannot recurse (`RBS_SHIM_ACTIVE` guard).
- Worktrees are mirrored at identical absolute paths via gitignore-filtered
  rsync; `--delete` is scoped to the workspace root.

## Quick start

Both machines need rustup, the same pinned toolchain, [kache], rsync, and an
S3-compatible store for kache (a MinIO container works: see
[deploy/minio.md](deploy/minio.md)). Then:

```sh
cargo build --release
cp target/release/rbs ~/.local/bin/rbs
rbs setup --role laptop --remote-host <host>   # provisions this machine and the remote
export PATH="$HOME/.local/share/rbs/shim:$PATH"  # put in agent environments / shell rc
rbs doctor --remote                            # verify everything
```

Escape hatches: `RBS_MODE=remote|local|plain`, `RBS_LOCAL=1`,
`RBS_LOG=info` to see routing decisions, `.rbs.toml` for per-project overrides.
See [docs/OVERVIEW.md](docs/OVERVIEW.md) for the architecture and
[docs/features/](docs/features/) for per-subsystem docs.

## Status

Working end-to-end (local and remote modes, pull-and-link, doctor). Not yet
done: pty for interactive jobs, multi-agent load testing, Windows/macOS.

License: MIT.

[kache]: https://github.com/kunobi-ninja/kache
