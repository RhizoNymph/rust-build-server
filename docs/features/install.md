# install — Makefile for a local install and Claude Code sandbox setup

## Scope
A top-level `Makefile` that takes a checkout to a working rbs client on this
machine. It builds and installs the binary, runs `rbs setup` (units, kache
config and the shim hardlink), puts the shim on PATH, and merges the
exclusions Claude Code's Bash sandbox needs so that agents' cargo calls can
reach the build server. It also wraps `rbs doctor` and `rbs bootstrap`.

Non-scope: installing rustup, kache, jq or ssh keys (`make deps` only checks
for them); provisioning other hosts (that is `rbs bootstrap`, wrapped by
`make fleet`); MinIO (`deploy/minio.md`); changing what `rbs setup` itself
does.

## Targets

| target | does | writes |
|---|---|---|
| `help` (default) | lists targets | — |
| `deps` | checks `cargo rustup rsync ssh jq systemctl kache` are on PATH | — |
| `build` | `RBS_MODE=plain cargo build --release --locked -p rbs` | `target/release/rbs` |
| `install` | `deps build`, then installs to `$(BIN_DIR)/rbs.new` and `mv -f` over `rbs`, runs `rbs setup --role $(ROLE) [--remote-host $(REMOTE_HOST)]`, restarts `rbs-server` if enabled | `~/.local/bin/rbs`, whatever `setup` writes |
| `restart` | `systemctl --user restart rbs-server` when the unit is enabled | — |
| `shim-path` | appends the marked shim `export PATH` block to the login shell's rc unless a line already mentions `.local/share/rbs/shim` | `~/.bashrc` or `~/.zshrc` |
| `sandbox` | merges rbs's patterns into `.sandbox.excludedCommands`, keeping existing entries, deduplicated and sorted | `~/.claude/settings.json` (+ `.bak`) |
| `sandbox-show` | prints `.sandbox.excludedCommands` | — |
| `agent-guidance` | optional; appends `deploy/claude/agent-guidance.md` between HTML-comment markers | `~/.claude/CLAUDE.md` |
| `doctor` | `rbs doctor --remote` | — |
| `fleet-plan` / `fleet` | `rbs bootstrap --dry-run` / `install` then `rbs bootstrap` | remote hosts |
| `all` | `install shim-path sandbox doctor` | as above |

Overridable variables: `ROLE` (client), `REMOTE_HOST`, `BIN_DIR`, `SHELL_RC`,
`CLAUDE_SETTINGS`, `CLAUDE_MD`, `SANDBOX_CARGO`, `SANDBOX_HELPERS`.

## Data / control flow
1. `build` runs with `RBS_MODE=plain` so a shim already on PATH doesn't ship
   the build of the shim's own binary to the remote; the binary is needed here.
2. `install` stages the binary and renames it into place. A running
   `rbs server` maps the old inode, so copying over the file in place fails
   with `Text file busy`; rename swaps the directory entry. `rbs setup` then
   re-creates the shim hardlink, which still points at the old inode after
   the rename, and the server is restarted to exec the new binary.
3. `sandbox` builds the pattern list in jq from two word lists:
   `cargo <sub>` and `cargo <sub> *` for each of `SANDBOX_CARGO`
   (build check test clippy bench doc nextest), and `<h> *` for each of
   `SANDBOX_HELPERS` (cd tail head grep). It unions them with the existing list,
   writes a temp file and renames it over the settings file, so a jq failure
   leaves the settings untouched.

## Why the sandbox needs this
Claude Code runs each Bash command in bubblewrap, which blocks the shim's ssh
to the server and its connection to the local server's unix socket, so a
sandboxed `cargo build` silently falls back to plain local cargo. A command
listed in `excludedCommands` runs outside the sandbox, and so do its child
processes (ssh, rsync, kache), so the cargo patterns are enough and `ssh`/`rsync`
need no entry of their own. A compound command (`cd x && cargo build | tail`)
is only excluded when **every** part matches, and agents rarely run cargo
bare, hence the read-only helpers. Verified: `cd main && cargo check -p
rbs-proto | tail` reached node0 with the helpers listed; the same chain with
an extra `touch` stayed sandboxed and built locally.

## Files
- `Makefile` — the targets above.
- `deploy/claude/agent-guidance.md` — the CLAUDE.md note telling agents to
  run cargo bare or chained only with the excluded helpers.
- `docs/features/install.md` — this document.

## Invariants and constraints
- Every target is idempotent: reruns leave the rc file, settings and CLAUDE.md
  unchanged after the first run.
- `sandbox` never removes an existing pattern; `cargo run` is deliberately
  absent because it executes the built program, which should stay sandboxed.
- Recipes run under `.ONESHELL` with `bash -euo pipefail`. Recipe lines must
  not start with `+`, `-` or `@` beyond the first line (make strips them),
  and recipes must not invoke `$(MAKE)`, which would make `make -n` execute
  the whole one-shell recipe.
- The settings, rc and CLAUDE.md files are write-protected inside Claude
  Code's sandbox; the user runs these targets from a normal terminal or with
  the `!` prefix.
