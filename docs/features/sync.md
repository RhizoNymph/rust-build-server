# sync — worktree mirroring and artifact pull

## Scope
Push the workspace to the remote host before a job; pull kache artifacts after
a remote build. Non-scope: syncing `target/` in either direction (kache is the
transport), syncing cargo registry (cargo fetches on node0), choosing *when* to
sync (client).

## API (`crates/rbs-sync`)
```rust
pub struct RsyncPlan { rsync_path, root, host, excludes, force_include }
impl RsyncPlan { fn new(&rbs_config::Sync, root: &Path, host: &str) -> Self; fn argv(&self) -> Vec<OsString>; fn source(); fn destination() }
pub async fn push(cfg: &rbs_config::Sync, root: &Path, host: &str) -> Result<(), SyncError>
pub async fn pull(root: &Path) -> Result<(), SyncError>
pub async fn workspace_root(cwd: &Path) -> Result<PathBuf, SyncError>
pub enum SyncError { Spawn { program, source }, Failed { program, status, stderr }, NoWorkspace { cwd, reason } }
```

## Flow
- `push(cfg, root, host)` runs (via `tokio::process`, stdin null, output captured):
  ```
  <rsync_path> -a --delete --mkpath [--include=<force_include>…] --filter=':- .gitignore'
      --exclude=.git/ --exclude=target/ [--exclude=<extra_excludes>…] <root>/ <host>:<root>/
  ```
  `--include` entries come *first* so they beat both the `.gitignore` merge
  rule and every `--exclude` (rsync's first-match filter chain); this is how
  `[sync] force_include = [".env"]` ships a gitignored file. Both paths always
  carry exactly one trailing `/` (contents of root → contents of root).
  Non-zero exit → `SyncError::Failed` with rsync's trimmed stderr; the client
  logs it at `warn` and moves to the next backend. rsync stdout is logged at
  `debug`.
- `pull(root)` runs `kache sync --pull` in the workspace root (filters by
  `Cargo.lock`). The client calls it after a successful remote `build` when
  `policy.post_build = pull_and_link`, then runs local `cargo build` with the
  same args so `target/` is populated via cache hits + a local link.
- `workspace_root(cwd)` = parent of `cargo locate-project --workspace
  --message-format plain` run in `cwd`; not in a workspace → `NoWorkspace`.

## Files
- `crates/rbs-sync/src/plan.rs` — `RsyncPlan` and pure `argv()` (unit tested exactly, incl. ordering and slashes).
- `crates/rbs-sync/src/lib.rs` — `push`, `pull`, `workspace_root`, `SyncError`.

## Tests (hermetic)
argv exactness with and without includes/excludes; trailing-slash handling;
`workspace_root` against a temp workspace and a non-workspace; `push` against
a fake `rsync` script (failure carries stderr + status; success), and a typed
spawn error for a missing binary. No test touches the network or node0.

## Invariants
- `--delete` is scoped to the workspace root only; never touches anything above it.
- `.git/` is excluded: the remote worktree is a plain directory (git metadata not needed to build; avoids lock-file races with agents).
- `target/` is never synced in either direction.
- If rsync exits non-zero the job is not submitted to the remote.
