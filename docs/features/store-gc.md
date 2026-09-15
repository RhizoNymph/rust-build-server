# store-gc — size cap + LFU eviction for the shared kache S3 store

## Scope
Keep the shared MinIO bucket kache uploads to under a configurable size by
deleting the least-frequently-used pack/manifest pairs, and keep eviction
recency store-wide: active workspaces "touch" their store objects
(`rbs store-touch`) so machines that never build on node0 still register as
users. Non-scope: kache's local disk cache (kache prunes that itself), writing
to the kache index, choosing what kache uploads, MinIO administration.

## Store layout and inputs
- Objects live at `<prefix>/v3/packs/<crate_name>/<cache_key>.tar.zst` and
  `<prefix>/v3/manifests/<crate_name>/<cache_key>.json`; `cache_key` is 64
  lowercase hex chars. Pack + manifest for one key are evicted as a pair; a
  key with only one of the two is an orphan. Keys that do not match this
  layout count toward the total but are never deleted.
- S3 endpoint/bucket/prefix/profile come from `~/.config/kache/config.toml`
  (`[cache.remote]`; `type` must be `"s3"`, only `bucket` is mandatory).
  Credentials: `KACHE_S3_ACCESS_KEY`/`KACHE_S3_SECRET_KEY` env vars, else the
  named profile in `~/.aws/credentials`. Path-style addressing (MinIO).
- Usage frequency comes from kache's local sqlite index
  (`~/.cache/kache/index.db`, table `entries`: `cache_key`, `hit_count`,
  `last_accessed`), opened read-only (`?mode=ro`). GC runs on node0, where
  every remote build executes, so these hit counts are a global frequency
  view of the store. A missing OR unreadable index — including a schema a future kache no longer
  matches (rbs is tested against kache 0.14.x) — degrades to "everything has
  0 hits" with a loud warning, so eviction falls back to age-only ranking
  instead of guessing against a drifted private schema. It also degrades to "everything has 0 hits"
  with a warning.

## Eviction policy
With `[store] max_size_gib` (0 = disabled), `low_watermark_percent`,
`min_age_hours`:
1. If total listed bytes ≤ cap: no-op (junk cleanup below still applies).
2. Otherwise evict, in deterministic order, until total ≤ cap × watermark%:
   orphans first, then pairs by ascending `hit_count`, tiebreak ascending
   *effective recency*, final tiebreak `cache_key`. Effective recency =
   `max(index last_accessed, newest S3 last-modified)`, so a store-touch by
   another machine counts as use; keys absent from the index rank as
   `hit_count = 0` with effective recency = S3 last-modified.
3. Never evict a group whose effective recency is within `min_age_hours`
   (covers both packs kache just uploaded that the index may not describe
   yet, and groups recently used or touched anywhere); such objects are
   counted as `skipped_young`.
4. `*.touch-tmp` keys (intermediates of a crashed `rbs store-touch`) are junk:
   never counted toward the total, deleted once older than `min_age_hours`
   even when the store is under the cap. Young ones (a touch in flight) are
   left alone.

## store-touch — store-wide recency
The index on node0 only sees builds that ran there. A laptop building locally
through kache uses store objects node0 never observes: they look like
`hits = 0` with last-modified = upload time and age into eviction candidates.
Fix: after a successful compiling build the shim fires a detached
`rbs store-touch --workspace <dir>` (see docs/features/client.md), which:
1. Resolves the workspace root (`cargo locate-project --workspace`, via
   `rbs_sync::workspace_root`).
2. Checks the throttle stamp `~/.cache/rbs/touch/<fnv1a64(root)>.stamp`
   (16 hex chars): mtime younger than `touch_after_hours` and no `--force` →
   exit 0 doing nothing. `[store] touch = false` disables the run entirely.
3. Reads crate names from the workspace `Cargo.lock` (`[[package]] name`) and
   lists `<prefix>/v3/{manifests,packs}/<crate>/` for each — both the
   Cargo.lock spelling and the `-`→`_` normalized variant (a wrong guess
   lists as empty).
4. Touches every listed object older than `touch_after_hours` with a two-step
   server-side copy (S3 forbids a metadata-free self-copy):
   `copy(key -> key.touch-tmp)`, `copy(key.touch-tmp -> key)`,
   `delete(key.touch-tmp)`. ≤ 8 concurrent; per-object failures warn and
   continue; NotFound anywhere = raced with GC, fine.
5. Writes/refreshes the stamp only after a run with zero failures, and logs
   one info line: crates scanned, objects listed, touched, skipped-fresh,
   failures (`TouchReport`).

## Data / control flow
```
rbs store-gc [--dry-run] [--max-size-gib N]  (CLI, crates/rbs-client)
  └─▶ rbs_store::run_gc(GcOpts) -> anyhow::Result<GcReport>
        ├─ rbs_config::load(cwd).store  (+ max_size_gib override; 0 → return)
        ├─ kache_cfg::load_kache_config(~/.config/kache/config.toml)
        ├─ creds::resolve_credentials(env, ~/.aws/credentials, profile)
        ├─ s3::build_s3 → object_store AmazonS3 (path-style, http allowed)
        ├─ gc::list_objects(store, prefix) → Vec<RemoteObject{key,size,last_modified}>
        ├─ index::load_index_or_empty(~/.cache/kache/index.db) → UsageMap
        ├─ planner::plan(objects, index, GcParams) → GcPlan   (pure; evictions + junk)
        └─ gc::execute_plan(store, plan, dry_run, now) → GcReport
             dry-run: print plan (crate, key[..12], size, hits, age; junk lines), delete nothing
             else: delete plan objects + junk, ≤16 concurrent, NotFound tolerated

rbs store-touch [--workspace <dir>] [--force]  (CLI + shim post-build trigger)
  └─▶ rbs_store::run_touch(TouchOpts) -> anyhow::Result<TouchReport>
        ├─ rbs_config::load(dir).store  (touch=false → no-op)
        ├─ rbs_sync::workspace_root(dir)
        ├─ touch::stamp_fresh_on_disk(stamp, now, touch_after_hours)  (fresh & !force → no-op)
        ├─ touch::parse_cargo_lock(root/Cargo.lock) → crate names
        ├─ kache_cfg / creds / s3  (same path as run_gc)
        ├─ touch::list_prefixes(store, crate_prefixes(prefix, name)…)  (≤8 concurrent)
        ├─ touch::plan_touch(objects, now - touch_after_hours) → TouchPlan  (pure)
        ├─ touch::execute_touch(store, plan.touch)  (copy·copy·delete, ≤8 concurrent)
        └─ touch::write_stamp(stamp)  (only when failures == 0)
```
Scheduling: `rbs setup --role server` installs `rbs-store-gc.service`
(oneshot) + `rbs-store-gc.timer` (daily, 1h random delay, persistent) as
systemd user units and enables the timer. The client role installs neither.

## Files
- `crates/rbs-store/src/lib.rs` — `GcOpts`, `GcReport`, `TouchOpts`,
  `TouchReport` (+ `Display`s), `run_gc` / `run_touch` (the `anyhow`
  boundaries), `StorePaths` env resolution, `connect_store`.
- `crates/rbs-store/src/planner.rs` — pure planner: `RemoteObject`,
  `parse_object_key`, `GcParams::from_store_config`, `Eviction`, `EvictReason`,
  `GcPlan` (evictions + junk), `plan`, `TOUCH_TMP_SUFFIX`.
- `crates/rbs-store/src/gc.rs` — `list_objects`, `execute_plan`, `format_plan`,
  `GcError`.
- `crates/rbs-store/src/touch.rs` — `parse_cargo_lock`, `crate_prefixes`,
  `stamp_path`/`stamp_is_fresh`/`write_stamp`, `plan_touch` (pure),
  `TouchStore` (injectable copy/delete) + `StoreOps`, `execute_touch`,
  `TouchError`.
- `crates/rbs-store/src/kache_cfg.rs` — `RemoteStore`, `parse_kache_config`,
  `load_kache_config`, `KacheConfigError`.
- `crates/rbs-store/src/creds.rs` — `Credentials` (redacting `Debug`),
  `resolve_credentials` (injectable env lookup + file text), `CredsError`.
- `crates/rbs-store/src/index.rs` — `UsageEntry`, `UsageMap`, `load_index`,
  `load_index_or_empty`, `parse_datetime`, `IndexError`.
- `crates/rbs-store/src/s3.rs` — `build_s3`, `S3Error`.
- `crates/rbs-client/src/main.rs` — `Cmd::StoreGc` / `Cmd::StoreTouch`
  dispatch; `RealHooks::spawn_touch` (detached spawn).
- `crates/rbs-client/src/shim.rs` — `COMPILING_SUBCOMMANDS`, `wants_touch`,
  the post-build / pre-exec trigger (docs/features/client.md).
- `crates/rbs-setup/src/setup.rs` — server-only unit install
  (`render_store_gc_service`, `STORE_GC_TIMER`).
- `deploy/systemd/rbs-store-gc.service`, `deploy/systemd/rbs-store-gc.timer` —
  unit templates (`{self_exe}` substituted in the service).

## Invariants and constraints
- The planner is pure and deterministic: listing, usage map and clock are
  inputs; equal inputs produce identical plans (BTreeMap grouping, total
  candidate ordering ending in `cache_key`).
- The kache index is only ever opened read-only; GC never writes to it.
- Pack and manifest for one cache key are always deleted together; orphans
  (single-sided keys) are preferred victims.
- `min_age_hours` guards against deleting objects kache is still writing or
  has not yet indexed — and, because it checks effective recency, against
  deleting anything used or touched anywhere within the window; `--dry-run`
  performs no deletion of any kind.
- A touch never loses data: the worst crash strands a `*.touch-tmp` copy,
  which GC junk-collects after `min_age_hours`. The touch stamp is refreshed
  only after a failure-free run, so failed touches retry on the next build.
- `rbs store-touch` never blocks a build: the shim spawns it fully detached
  (own process group, stdio null) and ignores spawn failures.
- Credentials are never logged, never in error text, and `Credentials`'
  `Debug` is redacted.
- Only node0 runs the timer: one evictor per store, using the only index with
  a global view. Errors exit non-zero (systemd surfaces the failed unit).
