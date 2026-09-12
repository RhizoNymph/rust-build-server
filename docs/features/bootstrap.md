# bootstrap — provision the whole fleet

## Scope
One command that brings every host of the fleet — one **server**, N **clients**
— to a working, verified state over ssh, and the same command that upgrades it
later. Non-scope: what a single host installs (that is `setup`, invoked
remotely per host), installing rustup or kache *locally*, running MinIO
(`deploy/minio.md`), creating ssh keys or trust.

## Command
`rbs bootstrap [--server H] [--clients a,b] [--dry-run] [--force]`
→ `rbs_setup::bootstrap(BootstrapOpts) -> Result<BootstrapReport>`

CLI flags override `[bootstrap]` in the layered config (`--clients` is
comma-separated; an empty list keeps the config value). Hosts are visited
**server first, then each client**, each host exactly once (a host named as
both keeps the server role). A failing host never aborts the others — the run
always visits every host and the report is the whole truth about the fleet.

Exit code is 0 only when every host got through every step it attempted.

## Per-host steps
Each step records `ok` / `skip` / `FAIL` plus a detail string; `skip` means
"already in the wanted state, or not applicable". Steps are attempted in order:

| # | step | what runs | skip / failure policy |
|---|---|---|---|
| 1 | `reach` | `ssh -o BatchMode=yes -o ConnectTimeout=5 <host> true` | FAIL ends this host; nothing else is attempted on it |
| 2 | `binaries` | `ssh <host> mkdir -p .local/bin`, then for `rbs` and `kache`: `scp <src> <host>:<dest>.new` + `ssh <host> mv -f <dest>.new <dest>` | missing **local** kache binary → note + continue (`rbs` still pushed); a failed push is a FAIL |
| 3 | `secrets` | `push_secrets = true`: `ssh <host> mkdir -p .config/rbs`, `scp ~/.config/rbs/minio.env <host>:.config/rbs/minio.env`, `ssh <host> chmod 600 …`. `false`: `ssh <host> test -f .config/rbs/minio.env` | `false` + absent on host → note with the exact `scp` command + skip; no local file → note + skip |
| 4 | `ssh-alias` | clients only: `ssh <client> "test -f .ssh/config && grep -q 'Host <server>' .ssh/config"`; if absent, append a block via `ssh <client> "mkdir -p .ssh && chmod 700 .ssh && printf '%s\n' 'Host <server>' '  HostName <ip>' '  StrictHostKeyChecking accept-new' >> .ssh/config"` | server role → skip; alias present → skip; no `HostName` for the server in the **local** `~/.ssh/config` → note + skip |
| 5 | `toolchain` | `ssh <host> "rustup toolchain list"`; when the wanted channel is absent, `ssh <host> "rustup toolchain install <ch> --profile minimal"` | channel unknown → note + skip; rustup unusable on the host → note carrying the rustup.rs one-liner + skip |
| 6 | `setup` | `ssh <host> .local/bin/rbs setup --role server\|client [--force]` | non-zero exit → FAIL with the captured output |
| 7 | `doctor` | `ssh <host> .local/bin/rbs doctor` (server) / `… doctor --remote` (clients, so their link to the server is checked too) | non-zero exit → FAIL listing the failing check names |

The wanted toolchain channel is `[bootstrap] toolchain` when set, else
`[toolchain] channel` of the nearest `rust-toolchain.toml` at or above the
workspace directory the command ran in. Cache keys include the rustc commit
hash, so a host on another channel can never reuse the fleet's artifacts.

`--dry-run` prints the planned action per host and executes **nothing** — not
even the reachability probe.

## Output
```
HOST  ROLE    reach  binaries  secrets  ssh-alias  toolchain  setup  doctor
node0 server  ok     ok        skip     skip       skip       ok     ok
lap   client  ok     ok        skip     ok         ok         ok     FAIL

failures:
  lap doctor: failed checks: kache-daemon

notes:
  lap: lap has no .config/rbs/minio.env; copy it yourself with `scp …`

2 hosts: 1 ok, 1 failed
```

## Secret handling
- `[bootstrap] push_secrets` defaults to **false**. MinIO credentials
  (`~/.config/rbs/minio.env`) travel to a host over ssh only when the operator
  turns it on explicitly; the default run merely *checks* whether the host has
  the file and, if not, prints the exact `scp` command to run by hand.
- A host without `minio.env` still gets its `setup` run: that run fails its
  kache step and `doctor` reports it. An honest reported failure is preferred
  over silently shipping credentials or over crashing the fleet run.
- When pushing is enabled the file is copied as a file — its contents are never
  read, parsed, logged or placed in a report — and `chmod 600` follows the copy.
- No step, note, log line or report field ever contains a secret value.

## Data / control flow
```
bootstrap(opts) ─▶ Paths::from_env(self_exe)
                   rbs_config::load(opts.workspace)
                   └─▶ bootstrap_with(&SystemRunner, &paths, &cfg, &opts) -> Result<BootstrapReport, BootstrapError>
                         ├─ server/clients = opts overrides else cfg.bootstrap
                         ├─ plan_hosts(server, clients)          (pure, server first, deduped)
                         ├─ resolve_channel(cfg, opts)           (cfg value | rust-toolchain.toml | Err(note))
                         ├─ opts.dry_run → Fleet::dry_run: print the plan, run nothing
                         └─ for each host: Fleet::run_host → HostReport { steps, notes }
render_report(&report) ─▶ table + failures + notes + summary; exit 0 iff report.ok()
```
Every external process goes through `Runner::run`, every path through `Paths`,
so `bootstrap_with` is fully testable with `FakeRunner` + a tempdir `Paths` —
no test spawns a process or touches the network.

## Files
- `crates/rbs-setup/src/bootstrap.rs` — `BootstrapOpts`, `BootstrapError`,
  `Step`, `StepStatus`, `HostReport`, `STEPS`, `bootstrap_with`,
  `render_report`, and the pure helpers `plan_hosts`, `ssh_config_hostname`,
  `parse_toolchain_channel`, `find_toolchain_file`, `toolchain_installed`,
  `doctor_failed_checks`.
- `crates/rbs-setup/src/lib.rs` — `BootstrapReport` (+ `ok()`), `bootstrap`.
- `crates/rbs-setup/src/bootstrap_tests.rs` — 30 tests (argv assertions per
  step, fleet behaviour, the pure parsers).
- `crates/rbs-config/src/lib.rs` — the `[bootstrap]` schema (`Bootstrap`).
- `crates/rbs-client/src/main.rs` — `Cmd::Bootstrap` → `rbs_setup::bootstrap`,
  prints `render_report`, exits 1 when `!report.ok()`.

## Invariants and constraints
- **Idempotent.** Rerunning on a healthy fleet changes nothing except
  re-pushing the binaries — which is exactly the upgrade path: build, then
  `rbs bootstrap` to roll the new `rbs` (and `kache`) out to every host.
- Binaries are never written in place: a running `rbs server` holds its own
  binary open, so the push stages `<path>.new` and `mv -f`s it over.
- Reachability is the only short-circuit. Every other step failure is recorded
  and the host continues, so one run surfaces every problem at once.
- A note is for something the operator must do by hand; it never implies the
  step succeeded, and it never carries a secret.
- `rbs`/`kache` on a host always live at `.local/bin/…` and are invoked by
  explicit path: a non-interactive ssh PATH has no `~/.local/bin`.
- The fleet shares one toolchain channel; `bootstrap` only ever *adds* a
  toolchain to a host, never switches its default or removes one.
- `BootstrapReport::ok()` is the AND over hosts; a host is ok when no attempted
  step failed. An empty report is ok.
