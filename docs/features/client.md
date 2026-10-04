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
- `rbs setup [--role client|server] [--remote-host H] [--force]` → `rbs_setup::setup` with `self_exe = current_exe()` (`laptop`/`node0` still parse as `client`/`server`).
- `rbs bootstrap [--server H] [--clients a,b] [--dry-run] [--force]` → `rbs_setup::bootstrap` with `workspace = cwd`; prints the per-host table from `rbs_setup::render_report`, exit 1 if `!report.ok()`.
- `rbs doctor [--remote]` → `rbs_setup::doctor`, prints `ok  /FAIL name detail` rows, exit 1 if `!report.ok()`.

Logging: `tracing-subscriber` to stderr, filter from `RBS_LOG` (default `warn`),
every line prefixed `rbs: ` (custom `MakeWriter`) so agent transcripts show why
a build ran where it did.

## Subcommand detection (`ShimInput::subcommand`)
Cargo's own grammar is `cargo [+toolchain] [flags] <subcommand> [args]`: a
`+<name>` toolchain override, if present, can *only* appear at `argv[1]`
(immediately after the program name — this is a rustup proxy convention, not
cargo's own parser, and rustup itself only ever looks at that one position).
Everything downstream that needs "the subcommand" — `is_passthrough`, the
`local_subcommands` bypass, `COMPILING_SUBCOMMANDS`/`wants_touch`, and the
post-build `== Some("build")` gate — goes through `ShimInput::subcommand()`,
which:
1. Checks `argv[1]` for a `+` prefix (`ShimInput::toolchain_override`,
   computed once in `from_env`/stored on the struct). If present, that token
   is the override (without the `+`), and the "subcommand position" shifts
   to `argv[2]`.
2. Returns whatever is at the subcommand position — the real subcommand, or
   a leading flag (`-v`, `--locked`, …) if the invocation has one there.

Leading flags are handled identically whether or not an override precedes
them: `is_passthrough` already treats **any** token at the subcommand
position that starts with `-` as "run the real cargo locally, don't try to
parse further" (this predates toolchain-override support and is unchanged;
see the passthrough rule below). So `cargo +stable -v build` and `cargo -v
build` both resolve to a leading-flag token and both go straight to the real
cargo — the override, if any, rides along in the full `argv` that gets
forwarded, so it is still honoured by whichever cargo actually runs it. Only
`cargo +<toolchain> <subcommand> …` with no intervening flags reaches the
normal subcommand-aware logic below with the correct subcommand.

**Before this detection existed** (fixed in the `+toolchain`-override PR),
`subcommand()` always returned `argv[1]` unconditionally. For any
`+toolchain`-prefixed invocation this returned the override token itself
(e.g. `"+stable"`), which is never in `PASSTHROUGH_SUBCOMMANDS` and never
`"build"`, so: `cargo +toolchain install` was wrongly **not** treated as
passthrough (it got submitted as a remote job instead of running locally,
so e.g. `cargo install --root /tmp/x` wrote its output on the *remote*
host and the local directory never saw it); `cargo +toolchain run` skipped
the `local_subcommands` bypass; and `cargo +toolchain build`'s post-build
pull-and-link gate (`sub == Some("build")`) never matched, so a successful
remote build was never pulled back or locally relinked.

## Shim control flow (`shim::run`)
```
0. if RBS_SHIM_ACTIVE=1 in env → exec real cargo immediately (recursion guard; main.rs)
1. cfg = rbs_config::load(cwd); input = ShimInput::from_env (cwd, argv=["cargo",…],
   env = filter_env(allowlist), identity = hostname/pid/RBS_LABEL, tty = stdout isatty,
   toolchain_override = the "+<name>" token at argv[1], if any)
2. mode == plain → exec real cargo
   passthrough (cargo policy only): no subcommand, a leading flag (`-V`, `--list`),
   or `ShimInput::subcommand()` ∈ PASSTHROUGH_SUBCOMMANDS (metadata, locate-project,
   fmt, add, update, clean, install, …) → exec real cargo (full argv, override
   included). Structural guard: `cargo -V`/`cargo metadata` from any tool
   (including rbs's own fingerprinting) can never become a job.
   subcommand() ∈ policy.local_subcommands (cargo policy only) → exec real cargo
3. fp = rbs_toolchain::fingerprint(cwd, input.toolchain_override.as_deref())
   (auto: failure → warn + plain; forced: exit 1). An explicit override makes
   `rustc`/`cargo` get invoked as `rustc +<name> -vV` / `cargo +<name> -V`
   instead of the directory default — see `docs/features/toolchain.md`.
4. if mode ∈ {auto, remote} && remote.enabled:
     conn = SshTransport.connect() ; probe(conn) = Hello + Status, both under
     connect_timeout_ms; RTT = the Status round trip (Hello pays the ssh
     session handshake, ~200 ms even on a LAN, and must not count). The handshaken conn is kept for Submit.
5. decision = backend::select(mode, probe, local configured, cfg)   # pure table
   every skipped backend is logged at warn with its reason; empty chain → exit 1
6. for backend in decision.chain:
     remote: root = rbs_sync::workspace_root(cwd); rbs_sync::push(root, host)
             (either failing → warn, next backend); submit on the kept conn.
             JobRequest carries both `toolchain` (the fingerprint from step 3)
             and `toolchain_override` (the raw "+<name>", if any) so the
             server fingerprints the same override on its own mirrored cwd
             (`Runner::check_toolchain`) before comparing.
     local:  LocalTransport.connect() (autostart: spawn `<current_exe> server --socket P`
             detached — own process group, stdio null — and retry ≤3 s); Hello; submit
     plain:  exec real cargo; return
   submit streams JobEvents: Stdout→stdout, Stderr→stderr, Queued → info
   "queued (position N)". SIGINT/SIGTERM → send Cancel{id}, keep draining to Exited.
     Exited(status)            → go to 7
     Rejected::ToolchainMismatch → ShimInput::hard_mismatch() (pinned workspace OR an
                                    explicit `+toolchain` override on this invocation):
                                    print rbs_toolchain::Mismatch, exit 1, NO fallback;
                                    neither: loud warning + next backend
     Rejected::Saturated / other / transport error → warn, next backend
7. if backend == remote && cargo policy && subcommand() == "build" && status.success()
      && post_build == pull_and_link:
     spawn store-touch (see 9); rbs_sync::pull(root) (failure → warn, continue);
     exec real `cargo build <same args>` (the full argv, override included, so
     the local relink still uses the same toolchain the remote build used)
8. exit with ExitStatus::as_process_exit_code() (Code(n) → n, Signal(s) → 128+s)
9. store-touch trigger: when cargo policy applies, `cfg.store.touch` is true and
   subcommand() ∈ COMPILING_SUBCOMMANDS (build, test, check, clippy, doc, bench, run),
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
- `crates/rbs-client/src/shim.rs` — `ShimInput` (incl. `toolchain_override`, `subcommand()`, `hard_mismatch()`), `toolchain_override_token(argv)`, `Hooks` trait (remote/local transports, `fingerprint(cwd, toolchain)`, workspace_root, push, pull, exec_local, spawn_touch, stdout, stderr, cancel_signal), `run(input, &hooks) -> i32`, `submit` (event pump + cancel forwarding), `COMPILING_SUBCOMMANDS`.
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
  `+toolchain` override matrix: `toolchain_override_token` only recognizes
  `argv[1]`; `+toolchain install`/`+toolchain run` resolve exactly like
  plain `install`/`run` (passthrough-local / local_subcommands bypass,
  override forwarded verbatim in the exec'd argv); `+toolchain build` pulls
  and links identically to plain `build` and fingerprints with the override
  (asserted via the `fingerprinted_with` test hook); a mismatch under
  `+toolchain` is a hard error even with `pinned = false`; `+toolchain -v
  build` and plain `-v build` are both passthrough (leading-flag handling is
  unchanged by override support).
- `real_cargo`: PATH walk skips the shim dir, CARGO_HOME / ~/.cargo preference, non-executables ignored, env of the exec'd command.
- `local`: fails fast without autostart, spawn failure reported, autostart connects once the (fake, python3) server listens.
- `status`: line formatting for ok / unavailable.

## Invariants
- The shim never recurses: `RBS_SHIM_ACTIVE=1` is set on jobs, on exec'd cargo, and on every subprocess `rbs_toolchain::fingerprint` spawns; if present at startup the shim execs real cargo before loading config. (Without the fingerprint guard the shim fork-bombed via `cargo -V` — found in integration, now covered by tests in rbs-toolchain and the passthrough table.)
- The remote `rbs` is invoked by explicit path (`[remote] remote_bin`, default `.local/bin/rbs`) because non-interactive ssh shells lack `~/.local/bin` on PATH.
- Exit code of the agent's `cargo` == exit code of the job (or 1 for rbs errors; rbs errors go to stderr prefixed `rbs:`).
- Interactive signals are forwarded as `Cancel`; the ssh child is killed when the connection is dropped.
- Toolchain mismatch is contract-aware: if the workspace has a `rust-toolchain.toml`/`rust-toolchain` file (`ShimInput.pinned`, via `rbs_toolchain::find_toolchain_file`) **or** this invocation named an explicit `cargo +<toolchain> …` override (`ShimInput.toolchain_override`) — `ShimInput::hard_mismatch()` ORs the two — mismatch is a hard error with no fallback; with neither, the workspace gets a loud stderr warning ("pin the workspace to build remotely") and falls through to the next backend. The two sources are kept as separate fields rather than one merged into the other: `pinned` answers "does this workspace have a pin file" and `toolchain_override` answers "did this specific invocation name a toolchain", and only `hard_mismatch()` combines them.
- A `+toolchain` override, when present, can only appear at `argv[1]` (cargo's own grammar); `ShimInput::subcommand()` skips it to find the real subcommand (or a leading flag) and is the single source every subcommand-dependent decision in `run()` reads from — see "Subcommand detection" above.
- All fallbacks are logged at `warn` with the reason.
- `rbs status` and the shim never contact the remote when `remote.enabled = false` or mode ∈ {local, plain}.
- **Known limitation (unrelated to `+toolchain` overrides):** the post-build pull-and-link step (`rbs_sync::pull` + local `cargo build`) relies on kache's cache keys matching between the remote build and the local relink, which in turn relies on `CARGO_TARGET_DIR` (if set) being the *same absolute path string* on both hosts — true by construction for the default `target/` inside the (mirrored-path) workspace root, and still true for an explicit `CARGO_TARGET_DIR` as long as it resolves to the identical absolute path on both machines (e.g. an absolute path under the mirrored `$HOME`). `rbs-sync`'s rsync push only ever excludes a literal `target/`; a custom `CARGO_TARGET_DIR` directory name is not specially excluded or included. This was not changed by the `+toolchain` fix and was not reproducible as a distinct bug in testing — see `docs/features/sync.md`'s Invariants for the underlying assumption.
