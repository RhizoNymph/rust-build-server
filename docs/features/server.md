# server — resource-aware job server

## Scope
Accept `JobRequest`s over a unix socket, schedule them against a fixed
compute budget, run them in cgroup scopes, stream output back. Same binary
runs on node0 (big pool) and on the laptop (local fallback, small pool).
Non-scope: deciding where a job runs (client), remote transport (ssh),
pty allocation (`request.tty` is currently ignored; output is piped).

## Public API (`crates/rbs-server/src/lib.rs`)
```rust
pub struct ServerOpts { pub socket: Option<PathBuf>, pub tokens: Option<u32> }
pub async fn run_server(cfg: rbs_config::Config, opts: ServerOpts) -> anyhow::Result<()>;
pub async fn run_proxy(socket: PathBuf) -> anyhow::Result<()>;

// Additive, used by tests and embedders:
pub enum ScopeMode { Auto, Disabled }           // systemd scope wrapping policy
pub struct RunOptions { pub scope: ScopeMode }
pub async fn run_server_until(cfg, opts, run: RunOptions, shutdown: impl Future<Output = ()>) -> anyhow::Result<()>;
pub async fn bridge_to_socket(socket: &Path, input: impl AsyncRead, output: impl AsyncWrite) -> Result<(), ServerError>;
pub enum ServerError;                            // thiserror; anyhow only at run_* boundary
```
`run_server` = `run_server_until(.., RunOptions::default(), wait_for_signal())`
where the shutdown future resolves on SIGINT or SIGTERM.

## Control flow
```
run_server_until(cfg, opts, run, shutdown)
  ├─ socket = opts.socket ?? cfg.local_server.socket
  ├─ tokens = opts.tokens ?? cfg.server.tokens; 0 → max(1, nproc - reserve_cores)
  ├─ socket::bind: mkdir -p parent, unlink stale socket, bind, chmod 0600
  ├─ jobserver::JobServer::create(<socket dir>, tokens): mkfifo jobserver.fifo,
  │    open O_RDWR (keepalive), write N × '+'
  ├─ scope: ScopeMode::Auto → probe `systemd-run --user --scope --quiet -- true`;
  │    ok → Scope::Systemd{mem_max_gib}, else warn + Scope::None. Disabled → Scope::None
  ├─ serve loop (select): accept → spawn handle_connection; 2 s tick → Shared::poll
  │    (re-evaluate queue against fresh MemAvailable); shutdown → break
  └─ shutdown: abort connection tasks (drops job handles → SIGTERM groups),
       wait ≤10 s for job supervisors (TaskTracker), unlink socket, drop
       JobServer (unlinks FIFO)

handle_connection (socket.rs), one job per connection:
  Hello{version} ── != PROTOCOL_VERSION → Error(VersionMismatch), close
              ── not Hello first → Error(UnexpectedMessage), close
  Status  → ServerStatus{identity, Capacity{tokens_total, tokens_free, mem, load1, accepting}, queued, running, uptime}
  Submit  → id = next_id
            validate cwd absolute && is_dir           else Rejected::Internal
            rbs_toolchain::fingerprint(cwd) (spawn_blocking) vs request.toolchain
              via rbs_toolchain::check                 else Rejected::ToolchainMismatch
            scheduler.admit(id, priority, snapshot):
              Accept → runner.spawn → Started … Exited (spawn failure → Rejected::Internal)
              Queue{position} → Queued; wait for release (oneshot) → spawn
              Reject(Saturated) → Rejected::Saturated
  Cancel{id} → running: SIGTERM group, SIGKILL after 5 s → Exited{Signal(15)}
             → queued: dequeue → Exited{Signal(15)}
             → unknown id: Error(UnexpectedMessage)
  Second Submit while a job is active → Error(UnexpectedMessage)
  Client EOF → queued job withdrawn / running job cancelled

runner.rs (per job):
  argv = request.argv (wrapped: systemd-run --user --scope --quiet
           -p MemoryMax=<job_mem_max_gib>G -p CPUWeight=<priority.cpu_weight()> -- argv)
  cwd  = request.cwd
  env  = {PATH,HOME,USER,TERM,LANG,XDG_RUNTIME_DIR,DBUS_SESSION_BUS_ADDRESS from server} + request.env + MAKEFLAGS=CARGO_MAKEFLAGS=
         "--jobserver-auth=fifo:<fifo>" + RUSTC_WRAPPER=kache + RBS_SHIM_ACTIVE=1
  stdin=/dev/null, stdout/stderr piped in 16 KiB chunks → Stdout/Stderr events
  process_group(0) so the whole tree can be signalled
  job_timeout_secs (0 = none) → terminate; exit → SIGKILL group (stray children)
  exit status → ExitStatus::Code | ExitStatus::Signal

rbs proxy [--socket P]   # stdio ↔ unix socket bridge (proxy.rs), used as `ssh node0 rbs proxy`
```

## Files
- `crates/rbs-server/src/lib.rs` — `ServerOpts`, `RunOptions`, `ScopeMode`, `run_server`, `run_server_until`, `run_proxy`, `bridge_to_socket`, token resolution, signal wait.
- `crates/rbs-server/src/error.rs` — `ServerError` (thiserror).
- `crates/rbs-server/src/socket.rs` — `bind`, `Shared` (scheduler + runner + jobserver + waiters + job `TaskTracker`), `handle_connection` state machine (`Active::{None,Queued,Running}`).
- `crates/rbs-server/src/scheduler.rs` — pure `Scheduler { admit, finish, poll, remove_queued, position, accepting }`, `Limits: From<&rbs_config::Server>`, `Decision`.
- `crates/rbs-server/src/runner.rs` — `Runner { makeflags, scope: Scope, timeout, hostname }`, `validate_cwd`, `check_toolchain`, `spawn → JobHandle`, `CancelToken`, `probe_systemd_scope`, `KILL_GRACE`.
- `crates/rbs-server/src/jobserver.rs` — `JobServer::create(dir, n)`, `makeflags()`, `tokens_total()`, `tokens_free(running)`; unlinks on drop.
- `crates/rbs-server/src/sysinfo.rs` — `parse_mem_available`, `parse_load1`, `Snapshot`, `snapshot()`.
- `crates/rbs-server/src/proxy.rs` — `bridge(socket, input, output)`.
- `crates/rbs-server/tests/server_e2e.rs` — end-to-end over a real unix socket (unscoped).

## Invariants
- Tokens: cargo inherits the FIFO jobserver from `MAKEFLAGS`/`CARGO_MAKEFLAGS`; every cargo on the host shares one pool of `tokens` compile slots regardless of `-j`. The server never counts tokens for admission — admission is by job count + memory; tokens bound parallelism inside jobs.
- `tokens_free` is best effort: `tokens_total - running_jobs` (clamped at 0). No ioctl/FIONREAD probing (avoids `unsafe`). Informational only.
- Admission (`scheduler.rs`): accept iff `running < max_jobs` AND `MemAvailable > min_mem_available_gib` AND the queue is empty (freed capacity goes to the queue first). Queue ordered by `Priority::rank()` desc, then arrival. `queue.len() >= queue_limit` → `Rejected::Saturated`. The queue is re-polled every 2 s and whenever a job ends.
- `systemd-run` unavailable (probe fails at start) or failing to spawn at job time → `warn` once and run unscoped; never refuse work because of cgroups. `ScopeMode::Disabled` skips the probe entirely (tests).
- The probe runs with exactly the job environment (`inherit_env`: `env_clear` + `INHERITED_ENV`), so it cannot pass while jobs fail. `XDG_RUNTIME_DIR`/`DBUS_SESSION_BUS_ADDRESS` are inherited because `systemd-run --user` needs the user bus (found in integration: every scoped job exited 1 with "Failed to connect to bus").
- A job's cwd must be absolute and an existing directory; otherwise `Rejected::Internal`. Toolchain is checked before admission, so `Rejected::ToolchainMismatch` is never preceded by `Queued`.
- Job event order per connection: `Queued?` → `Started` → (`Stdout`|`Stderr`)* → `Exited`, or `Rejected` terminal. Cancelling a queued job yields `Exited{Signal(15)}`.
- A released queued job whose connection vanished immediately returns its slot (no leak); a running job's slot is released by the supervisor task regardless of the connection's fate.
- Socket is mode 0600; FIFO is `<socket dir>/jobserver.fifo` mode 0600; both are removed on clean shutdown.
- All process I/O goes through tokio (`tokio::process`, pumps as tasks); toolchain fingerprinting uses `spawn_blocking`. No std threads, no `unsafe`.
- Logging: `tracing` key-value fields. info: listen/stop, job accepted/queued/started/exited/cancelled; warn: fallbacks, rejections, protocol mismatches; debug: per-event, per-connection.

## Deviations / not yet done
- `request.tty` is accepted but ignored (no pty; output is always piped).
- One job per connection; a second `Submit` on the same connection is a protocol error.
- `tokens_free` is derived, not measured (see above).
