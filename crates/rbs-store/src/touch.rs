//! Workspace store-touch: refresh S3 last-modified on the objects a workspace
//! uses, so store-wide recency is visible to the GC on the build server.
//!
//! Machines that build through kache without going through node0 never bump
//! kache's index there; without a touch their hot artifacts look like
//! `hits = 0` uploaded-long-ago groups and age into eviction candidates.
//! `rbs store-touch` lists the objects for every crate in the workspace's
//! `Cargo.lock` and rewrites the stale ones with a two-step server-side copy
//! (S3 forbids a metadata-free self-copy): `key -> key.touch-tmp -> key`,
//! then deletes the intermediate. Runs are throttled per workspace with a
//! stamp file under `~/.cache/rbs/touch/`.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use futures::{StreamExt, stream};
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjPath};
use serde::Deserialize;
use thiserror::Error;
use tracing::{debug, warn};

use crate::gc::{GcError, list_objects};
use crate::planner::{RemoteObject, TOUCH_TMP_SUFFIX};

/// Touches in flight at once.
const TOUCH_CONCURRENCY: usize = 8;

#[derive(Debug, Error)]
pub enum TouchError {
    #[error("invalid Cargo.lock: {message}")]
    Lockfile { message: String },
}

#[derive(Debug, Deserialize)]
struct LockFile {
    #[serde(default)]
    package: Vec<LockPackage>,
}

#[derive(Debug, Deserialize)]
struct LockPackage {
    name: String,
}

/// Crate names from a `Cargo.lock` (`[[package]] name = "…"`), deduplicated.
pub fn parse_cargo_lock(text: &str) -> Result<BTreeSet<String>, TouchError> {
    let file: LockFile = toml::from_str(text).map_err(|e| TouchError::Lockfile {
        message: e.to_string(),
    })?;
    Ok(file.package.into_iter().map(|p| p.name).collect())
}

/// List prefixes holding one crate's store objects. kache keys objects by the
/// rustc crate name (`-` → `_`), but the evidence is indirect, so both the
/// Cargo.lock spelling and the normalized one are listed (a missing prefix
/// lists as empty).
pub fn crate_prefixes(store_prefix: Option<&str>, crate_name: &str) -> Vec<String> {
    let base = store_prefix.map(|p| format!("{p}/")).unwrap_or_default();
    let mut variants = vec![crate_name.to_string()];
    let normalized = crate_name.replace('-', "_");
    if normalized != crate_name {
        variants.push(normalized);
    }
    variants
        .iter()
        .flat_map(|v| {
            [
                format!("{base}v3/manifests/{v}"),
                format!("{base}v3/packs/{v}"),
            ]
        })
        .collect()
}

/// FNV-1a 64-bit; stable across releases so stamp names never migrate.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Per-workspace throttle stamp: `<cache_dir>/rbs/touch/<16-hex>.stamp`.
pub fn stamp_path(cache_dir: &Path, workspace_root: &Path) -> PathBuf {
    let hash = fnv1a64(workspace_root.as_os_str().as_encoded_bytes());
    cache_dir.join(format!("rbs/touch/{hash:016x}.stamp"))
}

/// A stamp younger than `max_age` throttles the run. A stamp from the future
/// (clock skew) counts as fresh rather than triggering constant re-touching.
pub fn stamp_is_fresh(mtime: SystemTime, now: SystemTime, max_age: std::time::Duration) -> bool {
    now.duration_since(mtime)
        .map(|d| d < max_age)
        .unwrap_or(true)
}

/// Read the stamp's mtime and apply [`stamp_is_fresh`]; no stamp = stale.
pub fn stamp_fresh_on_disk(stamp: &Path, now: SystemTime, max_age: std::time::Duration) -> bool {
    std::fs::metadata(stamp)
        .and_then(|m| m.modified())
        .map(|mtime| stamp_is_fresh(mtime, now, max_age))
        .unwrap_or(false)
}

/// Create/refresh the stamp (mtime = now). Only called after a clean run.
pub fn write_stamp(stamp: &Path) -> std::io::Result<()> {
    if let Some(parent) = stamp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(stamp, b"")
}

/// Which listed objects a touch run must rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TouchPlan {
    /// Keys to touch, sorted (deterministic across listing order).
    pub touch: Vec<String>,
    /// Objects already fresher than the threshold.
    pub skipped_fresh: u64,
}

/// Pure: pick the objects whose last_modified is at or before `cutoff`.
/// `*.touch-tmp` intermediates are never touched (GC junk-collects them).
pub fn plan_touch(objects: &[RemoteObject], cutoff: DateTime<Utc>) -> TouchPlan {
    let mut plan = TouchPlan::default();
    for obj in objects {
        if obj.key.ends_with(TOUCH_TMP_SUFFIX) {
            continue;
        }
        if obj.last_modified <= cutoff {
            plan.touch.push(obj.key.clone());
        } else {
            plan.skipped_fresh += 1;
        }
    }
    plan.touch.sort();
    plan
}

/// The two operations a touch needs, injectable so tests can record the
/// copy-copy-delete sequence.
pub(crate) trait TouchStore: Sync {
    fn copy(
        &self,
        from: &str,
        to: &str,
    ) -> impl Future<Output = Result<(), object_store::Error>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), object_store::Error>> + Send;
}

/// Adapter from any [`ObjectStore`] to [`TouchStore`].
pub(crate) struct StoreOps<'a>(pub &'a dyn ObjectStore);

impl TouchStore for StoreOps<'_> {
    async fn copy(&self, from: &str, to: &str) -> Result<(), object_store::Error> {
        ObjectStoreExt::copy(self.0, &ObjPath::from(from), &ObjPath::from(to)).await
    }
    async fn delete(&self, key: &str) -> Result<(), object_store::Error> {
        ObjectStoreExt::delete(self.0, &ObjPath::from(key)).await
    }
}

/// List every prefix (bounded concurrency) and flatten. Any listing error
/// fails the run: an unreachable store must not refresh the stamp.
pub(crate) async fn list_prefixes(
    store: &dyn ObjectStore,
    prefixes: &[String],
) -> Result<Vec<RemoteObject>, GcError> {
    let lists: Vec<Result<Vec<RemoteObject>, GcError>> = stream::iter(prefixes)
        .map(|p| list_objects(store, Some(p)))
        .buffer_unordered(TOUCH_CONCURRENCY)
        .collect()
        .await;
    let mut out = Vec::new();
    for l in lists {
        out.extend(l?);
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TouchOutcome {
    Touched,
    /// The object vanished mid-touch (raced with GC): fine.
    Gone,
    Failed,
}

/// Rewrite one object in place: `copy(key -> tmp)`, `copy(tmp -> key)`,
/// `delete(tmp)`. S3 refuses a metadata-free self-copy, hence the round trip.
async fn touch_one<S: TouchStore>(store: &S, key: &str) -> TouchOutcome {
    let tmp = format!("{key}{TOUCH_TMP_SUFFIX}");
    match store.copy(key, &tmp).await {
        Ok(()) => {}
        Err(object_store::Error::NotFound { .. }) => {
            debug!(key, "object gone before touch (raced with gc)");
            return TouchOutcome::Gone;
        }
        Err(e) => {
            warn!(key, error = %e, "touch copy-out failed");
            return TouchOutcome::Failed;
        }
    }
    let back = store.copy(&tmp, key).await;
    // Best-effort cleanup either way; a stranded tmp is GC junk, not data loss.
    match store.delete(&tmp).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
        Err(e) => warn!(key, error = %e, "failed to delete touch intermediate"),
    }
    match back {
        Ok(()) => TouchOutcome::Touched,
        Err(object_store::Error::NotFound { .. }) => {
            debug!(key, "intermediate gone before copy-back (raced with gc)");
            TouchOutcome::Gone
        }
        Err(e) => {
            warn!(key, error = %e, "touch copy-back failed");
            TouchOutcome::Failed
        }
    }
}

/// Touch every planned key with bounded concurrency, tolerating per-object
/// failures. Returns `(touched, failures)`.
pub(crate) async fn execute_touch<S: TouchStore>(store: &S, keys: &[String]) -> (u64, u64) {
    let outcomes: Vec<TouchOutcome> = stream::iter(keys)
        .map(|key| touch_one(store, key))
        .buffer_unordered(TOUCH_CONCURRENCY)
        .collect()
        .await;
    let count = |o: TouchOutcome| outcomes.iter().filter(|&&x| x == o).count() as u64;
    (count(TouchOutcome::Touched), count(TouchOutcome::Failed))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::time::Duration as StdDuration;

    use chrono::Duration;
    use object_store::memory::InMemory;

    use super::*;

    const LOCK: &str = r#"
version = 3

[[package]]
name = "serde"
version = "1.0.229"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "0000"
dependencies = ["serde_derive"]

[[package]]
name = "proc-macro2"
version = "1.0.101"

[[package]]
name = "rbs-store"
version = "0.1.0"
"#;

    #[test]
    fn parses_crate_names_from_cargo_lock() {
        let names = parse_cargo_lock(LOCK).expect("parse");
        assert_eq!(
            names,
            BTreeSet::from([
                "proc-macro2".to_string(),
                "rbs-store".to_string(),
                "serde".to_string(),
            ])
        );
    }

    #[test]
    fn empty_or_packageless_lockfile_yields_no_crates() {
        assert!(parse_cargo_lock("").expect("parse").is_empty());
        assert!(parse_cargo_lock("version = 3\n").expect("parse").is_empty());
    }

    #[test]
    fn malformed_lockfile_is_a_typed_error() {
        let err = parse_cargo_lock("[[package]\n").expect_err("must fail");
        assert!(matches!(err, TouchError::Lockfile { .. }));
    }

    #[test]
    fn crate_prefixes_cover_both_kinds_and_name_variants() {
        assert_eq!(
            crate_prefixes(Some("artifacts"), "serde"),
            vec![
                "artifacts/v3/manifests/serde".to_string(),
                "artifacts/v3/packs/serde".to_string(),
            ]
        );
        assert_eq!(
            crate_prefixes(None, "proc-macro2"),
            vec![
                "v3/manifests/proc-macro2".to_string(),
                "v3/packs/proc-macro2".to_string(),
                "v3/manifests/proc_macro2".to_string(),
                "v3/packs/proc_macro2".to_string(),
            ]
        );
    }

    #[test]
    fn stamp_path_is_stable_and_distinct_per_root() {
        let cache = Path::new("/home/u/.cache");
        let a = stamp_path(cache, Path::new("/w/a"));
        let b = stamp_path(cache, Path::new("/w/b"));
        assert_eq!(a, stamp_path(cache, Path::new("/w/a")), "deterministic");
        assert_ne!(a, b);
        let name = a.file_name().expect("name").to_string_lossy().into_owned();
        assert_eq!(name.len(), "0123456789abcdef.stamp".len());
        assert!(name.ends_with(".stamp"));
        assert!(a.starts_with("/home/u/.cache/rbs/touch"));
    }

    #[test]
    fn stamp_freshness_logic() {
        let now = SystemTime::UNIX_EPOCH + StdDuration::from_secs(100_000);
        let max_age = StdDuration::from_secs(3600);
        let fresh = now - StdDuration::from_secs(60);
        let stale = now - StdDuration::from_secs(3600);
        let future = now + StdDuration::from_secs(60);
        assert!(stamp_is_fresh(fresh, now, max_age));
        assert!(!stamp_is_fresh(stale, now, max_age), "== max_age is stale");
        assert!(stamp_is_fresh(future, now, max_age), "clock skew tolerated");
    }

    #[test]
    fn stamp_on_disk_missing_is_stale_written_is_fresh() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stamp = stamp_path(dir.path(), Path::new("/w"));
        let now = SystemTime::now();
        let hour = StdDuration::from_secs(3600);
        assert!(!stamp_fresh_on_disk(&stamp, now, hour), "missing = stale");
        write_stamp(&stamp).expect("write");
        assert!(
            stamp_fresh_on_disk(&stamp, now, hour),
            "just written = fresh"
        );
        assert!(
            !stamp_fresh_on_disk(&stamp, now, StdDuration::ZERO),
            "zero window = always stale"
        );
    }

    fn now() -> DateTime<Utc> {
        "2026-09-10T12:00:00Z".parse().expect("timestamp")
    }

    fn obj(key: &str, age_hours: i64) -> RemoteObject {
        RemoteObject {
            key: key.to_string(),
            size: 1,
            last_modified: now() - Duration::hours(age_hours),
        }
    }

    #[test]
    fn plan_touch_picks_stale_skips_fresh_and_tmp() {
        let cutoff = now() - Duration::hours(24);
        let objects = vec![
            obj("v3/packs/b/x.tar.zst", 48),
            obj("v3/packs/a/y.tar.zst", 24), // exactly at cutoff: stale
            obj("v3/manifests/a/y.json", 1), // fresh
            obj("v3/packs/a/z.tar.zst.touch-tmp", 999),
        ];
        let plan = plan_touch(&objects, cutoff);
        assert_eq!(
            plan.touch,
            vec![
                "v3/packs/a/y.tar.zst".to_string(),
                "v3/packs/b/x.tar.zst".to_string(),
            ],
            "stale keys, sorted; tmp never touched"
        );
        assert_eq!(plan.skipped_fresh, 1);
    }

    /// Records the operation sequence while delegating to a real `InMemory`
    /// store; `fail` keys error on copy without being NotFound.
    struct Recording {
        inner: InMemory,
        ops: Mutex<Vec<String>>,
        fail: BTreeSet<String>,
    }

    impl Recording {
        fn new(inner: InMemory) -> Self {
            Self {
                inner,
                ops: Mutex::new(Vec::new()),
                fail: BTreeSet::new(),
            }
        }
        fn ops(&self) -> Vec<String> {
            self.ops.lock().unwrap_or_else(|e| e.into_inner()).clone()
        }
        fn record(&self, op: String) {
            self.ops.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        }
    }

    impl TouchStore for Recording {
        async fn copy(&self, from: &str, to: &str) -> Result<(), object_store::Error> {
            self.record(format!("copy {from} -> {to}"));
            if self.fail.contains(from) {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "injected failure".into(),
                });
            }
            StoreOps(&self.inner).copy(from, to).await
        }
        async fn delete(&self, key: &str) -> Result<(), object_store::Error> {
            self.record(format!("delete {key}"));
            StoreOps(&self.inner).delete(key).await
        }
    }

    async fn seed(store: &InMemory, keys: &[&str]) {
        for k in keys {
            store
                .put(&ObjPath::from(*k), vec![7u8].into())
                .await
                .expect("put");
        }
    }

    async fn keys_in(store: &InMemory) -> BTreeSet<String> {
        list_objects(store, None)
            .await
            .expect("list")
            .into_iter()
            .map(|o| o.key)
            .collect()
    }

    #[tokio::test]
    async fn touch_is_copy_copy_delete_per_object() {
        let inner = InMemory::new();
        let key = "artifacts/v3/packs/serde/aa.tar.zst";
        seed(&inner, &[key]).await;
        let store = Recording::new(inner);
        let (touched, failures) = execute_touch(&store, &[key.to_string()]).await;
        assert_eq!((touched, failures), (1, 0));
        assert_eq!(
            store.ops(),
            vec![
                format!("copy {key} -> {key}.touch-tmp"),
                format!("copy {key}.touch-tmp -> {key}"),
                format!("delete {key}.touch-tmp"),
            ]
        );
        assert_eq!(
            keys_in(&store.inner).await,
            BTreeSet::from([key.to_string()]),
            "no intermediate left behind"
        );
    }

    #[tokio::test]
    async fn missing_object_is_tolerated_not_a_failure() {
        let store = Recording::new(InMemory::new());
        let (touched, failures) =
            execute_touch(&store, &["v3/packs/serde/gone.tar.zst".to_string()]).await;
        assert_eq!((touched, failures), (0, 0), "raced with gc: fine");
        assert_eq!(store.ops().len(), 1, "stops after the NotFound copy-out");
    }

    #[tokio::test]
    async fn per_object_failure_does_not_stop_the_rest() {
        let inner = InMemory::new();
        let bad = "v3/packs/bad/aa.tar.zst";
        let good = "v3/packs/good/bb.tar.zst";
        seed(&inner, &[bad, good]).await;
        let mut store = Recording::new(inner);
        store.fail.insert(bad.to_string());
        let (touched, failures) = execute_touch(&store, &[bad.to_string(), good.to_string()]).await;
        assert_eq!((touched, failures), (1, 1));
        let ops = store.ops();
        assert!(ops.contains(&format!("copy {good}.touch-tmp -> {good}")));
    }
}
