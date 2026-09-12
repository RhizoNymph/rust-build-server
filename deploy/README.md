# Bringing up rbs on a fresh laptop + node0

Everything below is idempotent; rerun `rbs setup` freely. Nothing here touches
`main` of any repo, and no step needs root on either host.

## Prerequisites

On **both** hosts (same username, same `$HOME`):

- `rustup` with the toolchains your projects pin (see "Toolchain pinning").
- `kache` 0.14.2 at `~/.local/bin/kache`.
- `rsync`, `ssh`, and a systemd user session (`loginctl enable-linger $USER`
  on node0 so `rbs-server` keeps running when you are logged out).
- `ssh node0` works non-interactively from the laptop (keys, no prompt).

On **node0** only: the `rbs-minio` container (see [`minio.md`](minio.md)).

## 1. Secrets

Create `~/.config/rbs/minio.env` on node0 (format in `minio.md`) and copy it
to the laptop:

```sh
mkdir -p ~/.config/rbs
scp node0:.config/rbs/minio.env ~/.config/rbs/minio.env
chmod 600 ~/.config/rbs/minio.env
```

## 2. Build and install `rbs` on the laptop

```sh
cargo install --path crates/rbs-client --locked    # installs ~/.cargo/bin/rbs
```

## 3. Provision both hosts

```sh
rbs setup --role client --remote-host node0
```

This, in order:

1. writes `~/.config/rbs/config.toml` (client sizing) if absent — an existing
   file is diffed, never overwritten, unless you pass `--force`;
2. writes `~/.config/kache/config.toml` and the `[rbs]` profile in
   `~/.aws/credentials` (mode 600, other profiles untouched), then checks
   `kache stats` reports `Remote:     s3://kache/artifacts`;
3. installs and starts the `rbs-server` systemd **user** unit and runs
   `kache daemon install`;
4. creates the cargo shim at `~/.local/share/rbs/shim/cargo`;
5. copies the `rbs` binary to `node0:~/.local/bin/rbs` and runs
   `rbs setup --role server` there (the server's config has
   `[remote] enabled = false` and the larger `[server]` sizing).

For a fleet of more than one client, set `[bootstrap] server` / `clients` in
`~/.config/rbs/config.toml` and run `rbs bootstrap` instead: it does all of the
above on every host, plus the ssh alias and the pinned toolchain
(`docs/features/bootstrap.md`).

## 4. Put the shim on PATH for agents

Agents must see the shim **before** `~/.cargo/bin`. Add this to whatever
environment launches them (shell rc, the agent's env file, a wrapper script):

```sh
export PATH="$HOME/.local/share/rbs/shim:$PATH"
```

Interactive shells can keep the real cargo first; the shim is only needed where
`cargo` invocations should be scheduled by rbs.

## 5. Verify

```sh
rbs doctor --remote
```

Exit code is non-zero when any check fails, so agents and scripts can gate on
it. Checks: required binaries on PATH, kache remote configured, kache daemon
running, local server socket present, shim precedence, local toolchain
fingerprint, ssh reachability (with RTT), `rbs` present on node0, and the
node0 toolchain for the current directory matching the local one.

## Toolchain pinning

Cache keys include the rustc commit hash, so both hosts must resolve the
**same** toolchain for a workspace. Pin every project:

```toml
# rust-toolchain.toml at the workspace root
[toolchain]
channel = "1.95.0"            # or "nightly-2026-08-01" — never a bare "nightly"
components = ["rustfmt", "clippy"]
```

Then on both hosts: `rustup toolchain install 1.95.0`. `rbs doctor --remote`
reports a mismatch with both fingerprints and refuses to build remotely until
they agree.

## Escape hatches

- `RBS_LOCAL=1` or `RBS_MODE=local` — use the local `rbs server` only.
- `RBS_MODE=plain` — bypass rbs entirely (plain cargo + local kache).
- `[policy] local_subcommands` in `~/.config/rbs/config.toml` or a
  project-local `.rbs.toml` — subcommands that always run locally.

## Updating the binary

```sh
cargo install --path crates/rbs-client --locked
rbs setup --role client --remote-host node0   # re-links the shim, re-copies to node0, restarts nothing it doesn't need to
rbs bootstrap                                 # or: roll the new binary out to every host of the fleet
systemctl --user restart rbs-server           # on each host, to pick up the new server
```

## Troubleshooting

| Symptom | Check |
|---|---|
| `kache stats` shows no `Remote:` line | `~/.config/kache/config.toml` must be exactly the generated shape; kache ignores malformed TOML silently. Rerun `rbs setup --force`. |
| `remote-ssh` check slow or failing | You are probably on a slow WAN link; rbs falls back to local automatically when RTT > `max_rtt_ms`. |
| `server-socket` missing | `systemctl --user status rbs-server`, `journalctl --user -u rbs-server`. On node0 make sure lingering is enabled. |
| `shim-precedence` fails | The PATH line in step 4 is missing from the agent's environment. |
