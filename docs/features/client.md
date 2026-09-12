# client — cargo shim, backend selection, rbs CLI

## Scope
The user/agent-facing binary (`crates/rbs-client`, binary name `rbs`). Invoked
as `cargo` (shim) or `rbs` (CLI). Non-scope: scheduling (server), config schema
(config), rsync/kache invocation details (sync).

## Entry points
- `cargo <args>` via shim: `~/.local/share/rbs/shim/cargo` (hardlink to `rbs`), directory prepended to PATH in agent environments. `argv[0]` file name == `cargo` → shim mode with the remaining args.
- `rbs status` — probe each backend (remote via ssh, local via socket) and print one line each with RTT / tokens / mem / load / queued / running, or the failure reason. Exit 0 if any backend answered.
- `rbs exec [--mode auto|remote|local|plain] -- <cmd…>` — run an arbitrary command through the chain (cargo policy — `local_subcommands`, post-build — is *not* applied).
- `rbs server [--socket P] [--tokens N]` → `rbs_server::run_server(cfg, ServerOpts)`.
- `rbs proxy [--socket P]` → `rbs_server::run_proxy(socket)` (default `cfg.local_server.socket`).
- `rbs setup [--role laptop|node0] [--remote-host H] [--force]` → `rbs_setup::setup` with `self_exe = current_exe()`.
- `rbs doctor [--remote]` → `rbs_setup::doctor`, prints `ok  /FAIL name detail` rows, exit 1 if `!report.ok()`.

Logging: `tracing-subscriber` to stderr, filter from `RBS_LOG` (default `warn`),
every line prefixed `rbs: ` (custom `MakeWriter`) so agent transcripts show why
a build ran where it did.

## Shim control flow (`shim::run`)
```
0. if RBS_SHIM_ACTIVE=1 in env → exec real cargo immediately (recursion guard; main.rs)
1. cfg = rbs_config::load(cwd); input = ShimInput::from_env (cwd, argv=["cargo",…],
   env = filter_env(allowlist), identity = hostname/pid/RBS_LABEL, tty = stdout isatty)
2. mode == plain → exec real cargo
   passthrough (cargo policy only): no subcommand, a leading flag (`-V`, `--list`),
   or argv[1] ∈ PASSTHROUGH_SUBCOMMANDS (metadata, locate-project, fmt, add, update, clean, …)
   → exec real cargo. Structural guard: `cargo -V`/`cargo metadata` from any tool
   (including rbs's own fingerprinting) can never become a job.
   argv[1] ∈ policy.local_subcommands (cargo policy only) → exec real cargo
3. fp = rbs_toolchain::fingerprint(cwd)   (auto: failure → warn + plain; forced: exit 1)
4. if mode ∈ {auto, remote} && remote.enabled:
     conn = SshTransport.connect() ; probe(conn) = Hello + Status, both under
     connect_timeout_ms; RTT = the Status round trip (Hello pays the ssh
     session handshake, ~200 ms even on a LAN, and must not count). The handshaken conn is kept for Submit.
5. decision = backend::select(mode, probe, local configured, cfg)   # pure table
   every skipped backend is logged at warn with its reason; empty chain → exit 1
6. for backend in decision.chain:
     remote: root = rbs_sync::workspace_root(cwd); rbs_sync::push(root, host)
             (either failing → warn, next backend); submit on the kept conn
     local:  LocalTransport.connect() (autostart: spawn `<current_exe> server --socket P`
             detached — own process group, stdio null — and retry ≤3 s); Hello; submit
     plain:  exec real cargo; return
   submit streams JobEvents: Stdout→stdout, Stderr→stderr, Queued → info
   "queued (position N)". SIGINT/SIGTERM → send Cancel{id}, keep draining to Exited.
     Exited(status)            → go to 7
     Rejected::ToolchainMismatch → pinned workspace: print rbs_toolchain::Mismatch, exit 1, NO fallback;
                                    unpinned: loud warning + next backend
     Rejected::Saturated / other / transport error → warn, next backend
7. if backend == remote && cargo policy && argv[1] == "build" && status.success()
      && post_build == pull_and_link:
     spawn store-touch (see 9); rbs_sync::pull(root) (failure → warn, continue);
     exec real `cargo build <same args>`
8. exit with ExitStatus::as_process_exit_code() (Code(n) → n, Signal(s) → 128+s)
9. store-touch trigger: when cargo policy applies, `cfg.store.touch` is true and
   argv[1] ∈ COMPILING_SUBCOMMANDS (build, test, check, clippy, doc, bench, run),
   fire-and-forget `hooks.spawn_touch(dir)` → detached
   `<current_exe> store-touch --workspace <dir>` (own process group, stdio
   null; spawn failure = debug log). Server-backed jobs fire it after a
   successful exit (dir = cwd; the remote pull-and-link path fires with the
   already-resolved root). Every locally *exec'd* cargo replaces the process,
   so those paths fire it BEFORE the exec (the detached child outlives it):
   mode=plain, local_subcommands bypass, fingerprint-failure fallback, and the
   plain backend in the chain. Consequence: for exec'd plain builds the touch
   fires regardless of the build's eventual exit code. Never fired for
   passthrough subcommands, failed server-backed jobs, `rbs exec`
   (cargo_policy=false), or `store.touch = false`.
```
The job env always contains `RBS_SHIM_ACTIVE=1`; exec'd real cargo gets
`RUSTC_WRAPPER=kache` (unless already set) and `RBS_SHIM_ACTIVE=1`.

Real cargo resolution (`real_cargo::resolve`): `$CARGO_HOME/bin/cargo`, then
`$HOME/.cargo/bin/cargo` (rustup proxies, so `rust-toolchain.toml` is honoured),
then the first `cargo` on PATH — always skipping any candidate whose directory
is the running executable's directory (the shim dir). Exec replaces the process
(`CommandExt::exec`).

## Backend decision table (`backend::select`, pure)
| mode   | remote ok | local | chain                  |
|--------|-----------|-------|------------------------|
| auto   | yes       | any   | remote, (local), plain |
| auto   | no        | yes   | local, plain           |
| auto   | no        | no    | plain                  |
| remote | yes       | any   | remote                 |
| remote | no        | any   | none → error           |
| local  | any       | yes   | local                  |
| local  | any       | no    | none → error           |
| plain  | any       | any   | plain                  |

"remote ok" = `remote.enabled` ∧ probe succeeded ∧ `rtt <= max_rtt_ms` ∧
`capacity.accepting`. "local" = `local_server.enabled` (socket reachability is
discovered later in the chain; failure there falls through like any other).
`Decision { chain, skipped: Vec<Skip { backend, reason: SkipReason }> }`.

## Files
- `crates/rbs-client/src/main.rs` — argv[0] dispatch, clap CLI (incl. `rbs store-touch`), logging init, `RealHooks` (production `shim::Hooks`), signal future (SIGINT/SIGTERM).
- `crates/rbs-client/src/shim.rs` — `ShimInput`, `Hooks` trait (remote/local transports, fingerprint, workspace_root, push, pull, exec_local, spawn_touch, stdout, stderr, cancel_signal), `run(input, &hooks) -> i32`, `submit` (event pump + cancel forwarding), `COMPILING_SUBCOMMANDS`.
- `crates/rbs-client/src/backend.rs` — `Backend`, `RemoteProbe`, `SkipReason`, `Decision`, `select()`.
- `crates/rbs-client/src/transport.rs` — `Transport` / `Conn` traits (boxed futures, object-safe), `Framed<R,W>` newline-JSON duplex, `UnixTransport`, `SshTransport` (`ssh -o BatchMode=yes -o ConnectTimeout=<ceil secs> <host> rbs proxy`, child kept alive by the conn, killed on drop), `FakeTransport` (test-only: scripted replies, records sends, counts connects), `probe()`, `hello()`, `probe_with_timeout()`.
- `crates/rbs-client/src/local.rs` — `LocalTransport` (unix socket + autostart/retry).
- `crates/rbs-client/src/real_cargo.rs` — `resolve`, `find`, `command`, `exec`, `shim_active`, `SHIM_ACTIVE_ENV`.
- `crates/rbs-client/src/status.rs` — `rbs status` probing and line formatting.

(The planned `stream.rs` was folded into `shim::submit`; it is small and needs the hooks.)

## Tests (hermetic, `cargo test -p rbs`)
- `backend`: every row of the table, RTT boundary (== max ok, +1 skipped), disabled flags.
- `transport`: framing round trip over `tokio::io::duplex` (Hello/Status/Submit/Events incl. non-UTF-8 bytes), version mismatch, closed/unexpected, truncated frame at EOF, ssh argv.
- `shim` against `FakeTransport`: streaming to the right fds + exit code (incl. signal → 128+n), ToolchainMismatch → exit 1 with no local connect and no exec, Saturated → local, `local_subcommands` bypass, plain mode, pull-and-link after a successful remote build only, push failure → local, probe failure → local → plain, forced remote errors without fallback, not-accepting remote is never pushed to, `rbs exec` ignores cargo policy;
  store-touch trigger matrix: fired on success for compiling subcommands on
  remote/local/plain (pre-exec ordering asserted on exec'd paths), not on
  failed jobs, not for passthrough, not for `rbs exec`, not when
  `store.touch = false`.
- `real_cargo`: PATH walk skips the shim dir, CARGO_HOME / ~/.cargo preference, non-executables ignored, env of the exec'd command.
- `local`: fails fast without autostart, spawn failure reported, autostart connects once the (fake, python3) server listens.
- `status`: line formatting for ok / unavailable.

## Invariants
- The shim never recurses: `RBS_SHIM_ACTIVE=1` is set on jobs, on exec'd cargo, and on every subprocess `rbs_toolchain::fingerprint` spawns; if present at startup the shim execs real cargo before loading config. (Without the fingerprint guard the shim fork-bombed via `cargo -V` — found in integration, now covered by tests in rbs-toolchain and the passthrough table.)
- The remote `rbs` is invoked by explicit path (`[remote] remote_bin`, default `.local/bin/rbs`) because non-interactive ssh shells lack `~/.local/bin` on PATH.
- Exit code of the agent's `cargo` == exit code of the job (or 1 for rbs errors; rbs errors go to stderr prefixed `rbs:`).
- Interactive signals are forwarded as `Cancel`; the ssh child is killed when the connection is dropped.
- Toolchain mismatch is contract-aware: if the workspace has a `rust-toolchain.toml`/`rust-toolchain` file (`ShimInput.pinned`, via `rbs_toolchain::find_toolchain_file`), mismatch is a hard error with no fallback; an unpinned workspace gets a loud stderr warning ("pin the workspace to build remotely") and falls through to the next backend.
- All fallbacks are logged at `warn` with the reason.
- `rbs status` and the shim never contact the remote when `remote.enabled = false` or mode ∈ {local, plain}.
