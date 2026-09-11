# store-gc — size cap + LFU eviction for the shared kache S3 store

## Scope
Keep the shared MinIO bucket kache uploads to under a configurable size by
deleting the least-frequently-used pack/manifest pairs. Non-scope: kache's
local disk cache (kache prunes that itself), writing to the kache index,
choosing what kache uploads, MinIO administration.

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
  view of the store. A missing index degrades to "everything has 0 hits"
  with a warning.

## Eviction policy
With `[store] max_size_gib` (0 = disabled), `low_watermark_percent`,
`min_age_hours`:
1. If total listed bytes ≤ cap: no-op.
2. Otherwise evict, in deterministic order, until total ≤ cap × watermark%:
   orphans first, then pairs by ascending `hit_count`, tiebreak ascending
   `last_accessed`, final tiebreak `cache_key`. Keys absent from the index
   rank as `hit_count = 0` with `last_accessed` = S3 last-modified.
3. Never evict a group whose newest S3 last-modified is within
   `min_age_hours` (kache uploads packs the index may not describe yet);
   such objects are counted as `skipped_young`.

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
        ├─ planner::plan(objects, index, GcParams) → GcPlan   (pure)
        └─ gc::execute_plan(store, plan, dry_run, now) → GcReport
             dry-run: print plan (crate, key[..12], size, hits, age), delete nothing
             else: delete plan objects, ≤16 concurrent, NotFound tolerated
```
Scheduling: `rbs setup --role node0` installs `rbs-store-gc.service`
(oneshot) + `rbs-store-gc.timer` (daily, 1h random delay, persistent) as
systemd user units and enables the timer. The laptop role installs neither.

## Files
- `crates/rbs-store/src/lib.rs` — `GcOpts`, `GcReport` (+ `Display`), `run_gc`
  (the `anyhow` boundary), `GcPaths` env resolution.
- `crates/rbs-store/src/planner.rs` — pure planner: `RemoteObject`,
  `parse_object_key`, `GcParams::from_store_config`, `Eviction`, `EvictReason`,
  `GcPlan`, `plan`.
- `crates/rbs-store/src/gc.rs` — `list_objects`, `execute_plan`, `format_plan`,
  `GcError`.
- `crates/rbs-store/src/kache_cfg.rs` — `RemoteStore`, `parse_kache_config`,
  `load_kache_config`, `KacheConfigError`.
- `crates/rbs-store/src/creds.rs` — `Credentials` (redacting `Debug`),
  `resolve_credentials` (injectable env lookup + file text), `CredsError`.
- `crates/rbs-store/src/index.rs` — `UsageEntry`, `UsageMap`, `load_index`,
  `load_index_or_empty`, `parse_datetime`, `IndexError`.
- `crates/rbs-store/src/s3.rs` — `build_s3`, `S3Error`.
- `crates/rbs-client/src/main.rs` — `Cmd::StoreGc` dispatch.
- `crates/rbs-setup/src/setup.rs` — node0-only unit install
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
  has not yet indexed; `--dry-run` performs no deletion of any kind.
- Credentials are never logged, never in error text, and `Credentials`'
  `Debug` is redacted.
- Only node0 runs the timer: one evictor per store, using the only index with
  a global view. Errors exit non-zero (systemd surfaces the failed unit).
