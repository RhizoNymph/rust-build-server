# rbs — Rust Build Server

Transparent, resource-aware remote builds for `cargo`, designed for running
many parallel coding agents on one machine without melting it.

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
- The same server binary runs on the remote box (big pool) and on each client
  (small pool, the fallback).
- N clients building the same dependency tree compile each unit **once**:
  kache's per-key build locks dedupe concurrent identical work across the
  whole fleet.

## Requirements and assumptions

Read this before installing; rbs is deliberately opinionated:

- **Linux only**, with **systemd user sessions** (lingering enabled on the
  server host so it survives logout: `loginctl enable-linger $USER`).
- **The same username and home directory path on every host.** Worktrees are
  mirrored at identical absolute paths; there is no path remapping.
- **rustup** on every host. Toolchains must match between client and server
  for a build to run remotely; pin projects with `rust-toolchain.toml`
  (mismatch handling is contract-aware: a pinned workspace fails loudly, an
  unpinned one warns and builds locally).
- **SSH** between hosts with key auth (`BatchMode` must work), **rsync**, and
  an **S3-compatible store** for kache — a MinIO container on the server host
  works fine (see [deploy/minio.md](deploy/minio.md)).
- Login shells: the bootstrap PATH step covers **bash, sh and zsh** profiles
  (`~/.bash_profile` / `~/.profile` / `~/.zprofile`). Other shells: add the
  shim export line yourself.
- **kache compatibility: tested against kache 0.14.x.** rbs depends on
  kache's v3 store layout, and the store GC reads kache's local index
  (a private schema) for usage-based eviction. If a future kache changes that
  schema, GC degrades to age-only ranking with a loud warning rather than
  guessing — but treat kache upgrades as something to verify, not assume.

## Security model

rbs is a **trusted-network tool**. The job server listens on a user-owned unix
socket (mode 0600) and executes whatever argv it is sent; remote access to it
is exactly SSH access to that user. That is the intended boundary: anyone who
can ssh to the account can already run commands as it. Do not proxy the socket
beyond ssh-equivalent trust, and do not run the server as a privileged user.
Store credentials live in `~/.config/rbs/minio.env` (mode 600) and only ever
leave a machine when you explicitly set `[bootstrap] push_secrets = true`;
with the default `false`, bootstrap prints the `scp` command and lets you move
them yourself.

## Install

From a [release](https://github.com/RhizoNymph/rust-build-server/releases):

```sh
curl -fsSL -o rbs https://github.com/RhizoNymph/rust-build-server/releases/latest/download/rbs-x86_64-unknown-linux-gnu
install -m755 rbs ~/.local/bin/rbs
```

Or straight from git with cargo:

```sh
cargo install --git https://github.com/RhizoNymph/rust-build-server rbs
```

Or from a checkout: `cargo build --release && install -m755 target/release/rbs ~/.local/bin/rbs`.

## Quick start

One client + one server:

```sh
rbs setup --role client --remote-host <server>   # provisions this machine and the server
export PATH="$HOME/.local/share/rbs/shim:$PATH"  # put in agent environments / shell rc
rbs doctor --remote                              # verify everything
```

A whole fleet: describe it once in `~/.config/rbs/config.toml` and provision
(or later **upgrade** — bootstrap re-pushes binaries idempotently) everything
with one command:

```toml
[bootstrap]
server = "bigbox"
clients = ["laptop", "desk"]
```

```sh
rbs bootstrap --dry-run   # show the plan per host, run nothing
rbs bootstrap             # binaries, toolchain, ssh alias, shim PATH, setup + doctor per host
```

Escape hatches: `RBS_MODE=remote|local|plain`, `RBS_LOCAL=1`,
`RBS_LOG=info` to see routing decisions, `.rbs.toml` for per-project overrides.
See [docs/OVERVIEW.md](docs/OVERVIEW.md) for the architecture and
[docs/features/](docs/features/) for per-subsystem docs.

## Status

v0.1: working end-to-end and exercised daily on a four-machine fleet — local
and remote modes, pull-and-link, fleet bootstrap, store GC with store-wide
recency. Not yet done: pty for interactive jobs (`cargo run` executes remotely
with piped output; route it locally via `local_subcommands` if you need a
tty), Windows/macOS, path remapping for hosts with differing home directories.

License: MIT. Contributions: see [CONTRIBUTING.md](CONTRIBUTING.md).

[kache]: https://github.com/kunobi-ninja/kache
