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
| 5 | `toolchain` | **login shell**: `rustup toolchain list`; when the wanted channel is absent, `rustup toolchain install <ch> --profile minimal` | channel unknown → note + skip; rustup unusable on the host → note carrying the rustup.rs one-liner + skip |
| 6 | `shim-path` | `ssh <host> 'basename "$SHELL"'` = `zsh` → `~/.zprofile`; else `ssh <host> test -f .bash_profile` picks the target file, then `ssh <host> "grep -q '# >>> rbs shim >>>' <file>"`; when absent, append the marked block with `printf '%s\n' … >> <file>` | `shim_on_path = false` → skip + note carrying the manual line; already marked → `ok` ("already present") |
| 7 | `setup` | **login shell**: `.local/bin/rbs setup --role server\|client [--force]` | non-zero exit → FAIL with the captured output |
| 8 | `doctor` | **login shell**: `.local/bin/rbs doctor` (server) / `… doctor --remote` (clients, so their link to the server is checked too) | non-zero exit → FAIL listing the failing check names |

## Why the login shell (steps 5, 7, 8)
`ssh <host> <cmd>` runs a **non-interactive** shell, which sources no profile.
Its `$PATH` is sshd's default — `/usr/local/sbin:/usr/local/bin:/usr/sbin:
/usr/bin:/sbin:/bin:…` — with neither `~/.local/bin` nor `~/.cargo/bin` on it.
Observed against the real fleet: the remote `doctor` reported `binary:rbs`,
`binary:kache`, `binary:cargo`, `binary:rustup`, `toolchain` and
`remote-toolchain` as failures on every host although all of those binaries were
installed and working, and a host with rustup in `~/.cargo/bin` produced a bogus
"rustup is not usable" note. The steps whose *outcome is a statement about the
user's PATH* must therefore see the user's PATH, so they run as
`ssh <host> bash -lc '<cmd>'` (`login_shell_args`).

ssh does not preserve argv — it joins its words with spaces and the remote shell
re-parses the result — so `<cmd>` is passed as one shell-quoted word
(`shell_quote`: single quotes, with `'` written as `'\''`).

Steps that must **not** depend on profile state stay unwrapped: the reachability
probe (`true`), `mkdir -p`, `scp`, `mv -f`, `chmod`, the `test -f` secret check,
the ssh-config alias append, and every `shim-path` command (`test`, `grep`,
`printf` are all on sshd's default PATH). `rbs` and `kache` are still invoked by
explicit `.local/bin/…` path even inside the login shell — belt and braces, so a
host with an odd profile still gets provisioned.

The wanted toolchain channel is `[bootstrap] toolchain` when set, else
`[toolchain] channel` of the nearest `rust-toolchain.toml` at or above the
workspace directory the command ran in. Cache keys include the rustc commit
hash, so a host on another channel can never reuse the fleet's artifacts.

`--dry-run` prints the planned action per host and executes **nothing** — not
even the reachability probe.

## Output
```
HOST  ROLE    reach  binaries  secrets  ssh-alias  toolchain  shim-path  setup  doctor
node0 server  ok     ok        skip     skip       skip       ok         ok     ok
lap   client  ok     ok        skip     ok         ok         ok         ok     FAIL

failures:
  lap doctor: failed checks: kache-daemon

notes:
  lap: lap has no .config/rbs/minio.env; copy it yourself with `scp …`

2 hosts: 1 ok, 1 failed
```

## Shim on PATH (`shim-path`)
`setup` installs `~/.local/share/rbs/shim/cargo` but only *prints* the line that
puts it on PATH, so a freshly bootstrapped host had the shim and never used it:
every `cargo` there bypassed rbs, and `doctor` reported `shim-precedence` as
failed forever. `bootstrap` closes that gap by appending a marked block to the
host's shell profile:

```
# >>> rbs shim >>>
export PATH="$HOME/.local/share/rbs/shim:$PATH"
# <<< rbs shim <<<
```

- **Target file**: chosen by the host's LOGIN shell. zsh never reads
  `~/.profile`, so a `$SHELL` ending in `zsh` targets `~/.zprofile` (created by
  the append if absent). Otherwise `~/.profile` (login shells of both bash and
  sh source it), except when `~/.bash_profile` exists — bash reads that
  *instead of* `~/.profile`, so appending there would be silently ignored. The
  choice is made with `ssh <host> test -f .bash_profile`.
- **Idempotent**: the begin marker is grepped first and the block appended only
  when absent, so a rerun is a no-op and the block stays greppable for later
  edits or removal. Detail is `added to ~/<file>` / `already present in ~/<file>`.
- **Ordered before `setup`/`doctor`** on purpose: those two open *new* login
  shells, so the same run's `doctor` already sees the new PATH and can pass
  `shim-precedence`.
- **Applies to clients and the server.** The server runs agent builds too, and
  routing its `cargo` through rbs there is exactly as wanted as on a client; the
  shim is a no-op for anything that does not build. A single fleet-wide rule also
  keeps `shim-precedence` meaningful on every host of the report.
- `[bootstrap] shim_on_path = false` opts out: the step is `skip` and the note
  carries the exact line to add by hand.

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
  `doctor_failed_checks`, `shell_quote`, `login_shell_args`.
- `crates/rbs-setup/src/lib.rs` — `BootstrapReport` (+ `ok()`), `bootstrap`.
- `crates/rbs-setup/src/bootstrap_tests.rs` — 40 tests (argv assertions per
  step incl. what is and is not login-shell wrapped, `shim-path` idempotency and
  target-file choice, fleet behaviour, the pure parsers).
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
  explicit path even inside the login shell: the login PATH may be odd, and
  provisioning must not depend on it.
- A step whose result is a claim about the user's PATH runs under `bash -lc`;
  a step that must work regardless of profile state never does.
- `shim-path` only ever *appends* its marked block, never rewrites or reorders
  the host's profile, and never touches a file that already carries the marker.
- The fleet shares one toolchain channel; `bootstrap` only ever *adds* a
  toolchain to a host, never switches its default or removes one.
- `BootstrapReport::ok()` is the AND over hosts; a host is ok when no attempted
  step failed. An empty report is ok.
