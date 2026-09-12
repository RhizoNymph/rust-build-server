//! Listing and plan execution against an [`ObjectStore`].

use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt, stream};
use object_store::{ObjectStore, ObjectStoreExt, path::Path as ObjPath};
use thiserror::Error;
use tracing::{debug, info};

use crate::GcReport;
use crate::planner::{GcPlan, RemoteObject};

/// Deletions in flight at once.
const DELETE_CONCURRENCY: usize = 16;

#[derive(Debug, Error)]
pub enum GcError {
    #[error("listing the store failed: {0}")]
    List(#[source] object_store::Error),
    #[error("deleting {key} failed: {source}")]
    Delete {
        key: String,
        #[source]
        source: object_store::Error,
    },
}

/// List every object under `prefix` (`None` = whole bucket), sorted by key.
pub async fn list_objects(
    store: &dyn ObjectStore,
    prefix: Option<&str>,
) -> Result<Vec<RemoteObject>, GcError> {
    let prefix_path = prefix.map(ObjPath::from);
    let mut out: Vec<RemoteObject> = store
        .list(prefix_path.as_ref())
        .map_ok(|meta| RemoteObject {
            key: meta.location.to_string(),
            size: meta.size,
            last_modified: meta.last_modified,
        })
        .try_collect()
        .await
        .map_err(GcError::List)?;
    out.sort_by(|a, b| a.key.cmp(&b.key));
    Ok(out)
}

/// Human-readable plan, one line per object: crate, key prefix, size, hits, age.
pub fn format_plan(plan: &GcPlan, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    for eviction in &plan.evictions {
        for object in &eviction.objects {
            let age_hours = (now - object.last_modified).num_hours();
            out.push_str(&format!(
                "evict {crate_name} {key} {size:.1} MiB hits={hits} age={age_hours}h ({reason:?})\n",
                crate_name = eviction.crate_name,
                key = &eviction.cache_key[..12],
                size = object.size as f64 / (1024.0 * 1024.0),
                hits = eviction.hit_count,
                reason = eviction.reason,
            ));
        }
    }
    for object in &plan.junk {
        let age_hours = (now - object.last_modified).num_hours();
        out.push_str(&format!("junk {} age={age_hours}h\n", object.key));
    }
    out
}

/// Delete every planned object (bounded concurrency); already-gone objects are
/// fine (concurrent GC / kache activity). `dry_run` deletes nothing.
pub async fn execute_plan(
    store: &dyn ObjectStore,
    plan: &GcPlan,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<GcReport, GcError> {
    let report = GcReport {
        scanned: plan.scanned,
        total_bytes: plan.total_bytes,
        evicted: plan.evictions.iter().map(|e| e.objects.len() as u64).sum(),
        freed_bytes: plan.freed_bytes,
        skipped_young: plan.skipped_young,
        junk_deleted: plan.junk.len() as u64,
    };
    if dry_run {
        print!("{}", format_plan(plan, now));
        info!(
            evictions = plan.evictions.len(),
            freed_bytes = plan.freed_bytes,
            junk = plan.junk.len(),
            "dry run: nothing deleted"
        );
        return Ok(report);
    }
    let keys: Vec<&str> = plan
        .evictions
        .iter()
        .flat_map(|e| e.objects.iter().map(|o| o.key.as_str()))
        .chain(plan.junk.iter().map(|o| o.key.as_str()))
        .collect();
    stream::iter(keys)
        .map(|key| async move {
            match store.delete(&ObjPath::from(key)).await {
                Ok(()) => {
                    debug!(key, "deleted");
                    Ok(())
                }
                Err(object_store::Error::NotFound { .. }) => {
                    debug!(key, "already gone");
                    Ok(())
                }
                Err(source) => Err(GcError::Delete {
                    key: key.to_string(),
                    source,
                }),
            }
        })
        .buffer_unordered(DELETE_CONCURRENCY)
        .try_collect::<()>()
        .await?;
    info!(
        evicted = report.evicted,
        freed_bytes = report.freed_bytes,
        skipped_young = report.skipped_young,
        junk_deleted = report.junk_deleted,
        "gc complete"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use chrono::Duration;
    use object_store::memory::InMemory;

    use super::*;
    use crate::index::{load_index_or_empty, tests::seed_db};
    use crate::planner::{GcParams, plan};

    fn now() -> DateTime<Utc> {
        "2026-09-10T12:00:00Z".parse().expect("timestamp")
    }

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn pack_key(n: u8, crate_name: &str) -> String {
        format!("artifacts/v3/packs/{crate_name}/{}.tar.zst", key(n))
    }

    fn manifest_key(n: u8, crate_name: &str) -> String {
        format!("artifacts/v3/manifests/{crate_name}/{}.json", key(n))
    }

    /// Seed the in-memory store; `InMemory` stamps `now()` as last_modified,
    /// so tests inject their own listing with controlled timestamps.
    async fn seed(store: &InMemory, entries: &[(&str, u64)]) -> Vec<RemoteObject> {
        for (k, size) in entries {
            let bytes = vec![0u8; *size as usize];
            store
                .put(&ObjPath::from(*k), bytes.into())
                .await
                .expect("put");
        }
        entries
            .iter()
            .map(|(k, size)| RemoteObject {
                key: k.to_string(),
                size: *size,
                last_modified: now() - Duration::hours(48),
            })
            .collect()
    }

    async fn remaining_keys(store: &InMemory) -> BTreeSet<String> {
        list_objects(store, None)
            .await
            .expect("list")
            .into_iter()
            .map(|o| o.key)
            .collect()
    }

    fn params() -> GcParams {
        GcParams {
            max_bytes: 10,
            watermark_bytes: 8,
            min_age: Duration::hours(24),
            now: now(),
        }
    }

    #[tokio::test]
    async fn listing_maps_meta_and_sorts() {
        let store = InMemory::new();
        seed(&store, &[(&pack_key(2, "b"), 3), (&pack_key(1, "a"), 2)]).await;
        let listed = list_objects(&store, Some("artifacts")).await.expect("list");
        assert_eq!(listed.len(), 2);
        assert!(listed[0].key < listed[1].key);
        assert_eq!(listed[0].size, 2);
        // outside the prefix nothing is listed
        let empty = list_objects(&store, Some("elsewhere")).await.expect("list");
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn end_to_end_evicts_exactly_the_planned_objects() {
        let store = InMemory::new();
        let pk1 = pack_key(1, "serde");
        let mk1 = manifest_key(1, "serde");
        let pk2 = pack_key(2, "tokio");
        let mk2 = manifest_key(2, "tokio");
        // total 13 > 10; evicting the cold pair (key 1, 7 bytes) reaches 6 ≤ 8.
        let objects = seed(&store, &[(&pk1, 6), (&mk1, 1), (&pk2, 5), (&mk2, 1)]).await;

        // usage from a real (temp) sqlite index: key 2 is hot, key 1 cold.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.db");
        seed_db(
            &db,
            &[
                (key(1).leak(), "2026-09-01 00:00:00", 0),
                (key(2).leak(), "2026-09-09 00:00:00", 42),
            ],
        );
        let index = load_index_or_empty(&db).expect("index");

        let plan = plan(&objects, &index, &params());
        let report = execute_plan(&store, &plan, false, now()).await.expect("gc");

        assert_eq!(report.scanned, 4);
        assert_eq!(report.total_bytes, 13);
        assert_eq!(report.evicted, 2);
        assert_eq!(report.freed_bytes, 7);
        assert_eq!(report.skipped_young, 0);
        assert_eq!(
            remaining_keys(&store).await,
            BTreeSet::from([pk2, mk2]),
            "exactly the cold pack+manifest pair is gone"
        );
    }

    #[tokio::test]
    async fn dry_run_deletes_nothing_and_reports_the_same() {
        let store = InMemory::new();
        let pk1 = pack_key(1, "serde");
        let mk1 = manifest_key(1, "serde");
        let pk2 = pack_key(2, "tokio");
        let mk2 = manifest_key(2, "tokio");
        let objects = seed(&store, &[(&pk1, 6), (&mk1, 1), (&pk2, 5), (&mk2, 1)]).await;
        let index = load_index_or_empty(std::path::Path::new("/nonexistent/index.db"))
            .expect("missing index is an empty map");
        assert!(index.is_empty());

        let plan = plan(&objects, &index, &params());
        let report = execute_plan(&store, &plan, true, now()).await.expect("gc");
        assert_eq!(report.evicted, 2);
        assert_eq!(report.freed_bytes, 7);
        assert_eq!(
            remaining_keys(&store).await,
            BTreeSet::from([pk1, mk1, pk2, mk2]),
            "dry run must not delete"
        );
    }

    #[tokio::test]
    async fn deleting_an_already_missing_object_is_not_an_error() {
        let store = InMemory::new();
        let pk1 = pack_key(1, "serde");
        let objects = vec![RemoteObject {
            key: pk1.clone(),
            size: 11,
            last_modified: now() - Duration::hours(48),
        }];
        // listed but never stored: delete hits NotFound and is tolerated.
        let plan = plan(&objects, &Default::default(), &params());
        assert_eq!(plan.evictions.len(), 1);
        let report = execute_plan(&store, &plan, false, now()).await.expect("gc");
        assert_eq!(report.evicted, 1);
    }

    #[tokio::test]
    async fn stranded_touch_tmp_is_deleted_even_under_cap() {
        let store = InMemory::new();
        let pk1 = pack_key(1, "serde");
        let mk1 = manifest_key(1, "serde");
        let tmp = format!("{}.touch-tmp", pack_key(1, "serde"));
        let mut objects = seed(&store, &[(&pk1, 2), (&mk1, 1), (&tmp, 2)]).await;
        // A fresh tmp (touch in flight) must survive; only list it, not store it.
        let young_tmp = format!("{}.touch-tmp", manifest_key(2, "tokio"));
        objects.push(RemoteObject {
            key: young_tmp,
            size: 1,
            last_modified: now() - Duration::hours(1),
        });

        let plan = plan(&objects, &Default::default(), &params());
        assert!(plan.evictions.is_empty(), "under cap: no evictions");

        // dry run deletes nothing, reports the junk
        let report = execute_plan(&store, &plan, true, now()).await.expect("gc");
        assert_eq!(report.junk_deleted, 1);
        assert!(remaining_keys(&store).await.contains(&tmp));

        // real run removes exactly the stranded tmp
        let report = execute_plan(&store, &plan, false, now()).await.expect("gc");
        assert_eq!(report.junk_deleted, 1);
        assert_eq!(report.evicted, 0);
        assert_eq!(
            remaining_keys(&store).await,
            BTreeSet::from([pk1, mk1]),
            "pair kept, stranded tmp gone"
        );
    }

    #[test]
    fn format_plan_lists_junk() {
        let objects = vec![RemoteObject {
            key: format!("{}.touch-tmp", pack_key(1, "serde")),
            size: 1,
            last_modified: now() - Duration::hours(48),
        }];
        let p = plan(&objects, &Default::default(), &params());
        let text = format_plan(&p, now());
        assert!(text.contains("junk "), "{text}");
        assert!(text.contains(".touch-tmp age=48h"), "{text}");
    }

    #[test]
    fn format_plan_lists_crate_key_size_hits_age() {
        let objects = vec![RemoteObject {
            key: pack_key(1, "serde"),
            size: 3 * 1024 * 1024,
            last_modified: now() - Duration::hours(48),
        }];
        let p = plan(
            &objects,
            &Default::default(),
            &GcParams {
                max_bytes: 1,
                watermark_bytes: 0,
                min_age: Duration::hours(24),
                now: now(),
            },
        );
        let text = format_plan(&p, now());
        assert!(text.contains("serde"), "{text}");
        assert!(text.contains(&key(1)[..12]), "{text}");
        assert!(!text.contains(&key(1)[..13]), "key is truncated: {text}");
        assert!(text.contains("3.0 MiB"), "{text}");
        assert!(text.contains("hits=0"), "{text}");
        assert!(text.contains("age=48h"), "{text}");
    }
}
