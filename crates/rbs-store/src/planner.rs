//! Pure LFU eviction planning. No I/O: listing, usage and clock come in as data.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::index::UsageMap;

/// One object in the remote store, as reported by a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObject {
    /// Full object key, including any store prefix.
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Pack,
    Manifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    pub kind: ObjectKind,
    pub crate_name: String,
    pub cache_key: String,
}

/// Parse `<prefix>/v3/{packs,manifests}/<crate_name>/<cache_key>.{tar.zst,json}`.
/// Anything else (including keys outside `prefix`) is not GC-managed.
pub fn parse_object_key(prefix: &str, key: &str) -> Option<ParsedKey> {
    let rel = if prefix.is_empty() {
        key
    } else {
        key.strip_prefix(prefix)?.strip_prefix('/')?
    };
    let mut parts = rel.split('/');
    if parts.next()? != "v3" {
        return None;
    }
    let kind = match parts.next()? {
        "packs" => ObjectKind::Pack,
        "manifests" => ObjectKind::Manifest,
        _ => return None,
    };
    let crate_name = parts.next()?;
    let file = parts.next()?;
    if crate_name.is_empty() || parts.next().is_some() {
        return None;
    }
    let stem = match kind {
        ObjectKind::Pack => file.strip_suffix(".tar.zst")?,
        ObjectKind::Manifest => file.strip_suffix(".json")?,
    };
    let is_hex = |b: &u8| b.is_ascii_digit() || (b'a'..=b'f').contains(b);
    if stem.len() != 64 || !stem.as_bytes().iter().all(is_hex) {
        return None;
    }
    Some(ParsedKey {
        kind,
        crate_name: crate_name.to_string(),
        cache_key: stem.to_string(),
    })
}

/// Planner inputs derived from `[store]` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcParams {
    pub max_bytes: u64,
    /// Evict until the projected total is at or below this.
    pub watermark_bytes: u64,
    /// Objects modified within this window are never evicted.
    pub min_age: Duration,
    pub now: DateTime<Utc>,
}

impl GcParams {
    pub fn from_store_config(store: &rbs_config::Store, now: DateTime<Utc>) -> Self {
        const GIB: u64 = 1024 * 1024 * 1024;
        let max_bytes = u64::from(store.max_size_gib) * GIB;
        let percent = u64::from(store.low_watermark_percent.min(100));
        Self {
            max_bytes,
            watermark_bytes: max_bytes / 100 * percent + max_bytes % 100 * percent / 100,
            min_age: Duration::hours(i64::from(store.min_age_hours)),
            now,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// Pack without manifest or manifest without pack.
    Orphan,
    /// Evicted by the LFU ranking to get under the watermark.
    Lfu,
}

/// One planned eviction: all objects sharing a cache key (pack + manifest,
/// or a lone orphan object).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eviction {
    pub cache_key: String,
    pub crate_name: String,
    pub objects: Vec<RemoteObject>,
    pub hit_count: i64,
    pub last_accessed: DateTime<Utc>,
    /// Newest S3 last-modified among the objects (what the age guard checks).
    pub last_modified: DateTime<Utc>,
    pub reason: EvictReason,
}

impl Eviction {
    pub fn bytes(&self) -> u64 {
        self.objects.iter().map(|o| o.size).sum()
    }
}

/// The full deterministic plan for one GC run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GcPlan {
    /// Objects seen in the listing (including non-GC-managed keys).
    pub scanned: u64,
    /// Total listed bytes before eviction.
    pub total_bytes: u64,
    /// Evictions in execution order.
    pub evictions: Vec<Eviction>,
    /// Bytes the evictions will free.
    pub freed_bytes: u64,
    /// Objects protected by `min_age` while the store was still over target.
    pub skipped_young: u64,
}

#[derive(Debug, Default)]
struct Group {
    crate_name: String,
    pack: Option<RemoteObject>,
    manifest: Option<RemoteObject>,
}

impl Group {
    fn objects(self) -> Vec<RemoteObject> {
        self.pack.into_iter().chain(self.manifest).collect()
    }
}

/// Compute the eviction plan. Pure and deterministic: equal inputs give
/// byte-identical plans (BTreeMap grouping + total ordering on candidates).
pub fn plan(objects: &[RemoteObject], index: &UsageMap, params: &GcParams) -> GcPlan {
    let scanned = objects.len() as u64;
    let total_bytes: u64 = objects.iter().map(|o| o.size).sum();
    let mut plan = GcPlan {
        scanned,
        total_bytes,
        ..GcPlan::default()
    };
    if total_bytes <= params.max_bytes {
        return plan;
    }

    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for obj in objects {
        let Some(parsed) = parse_object_key("", key_without_prefix(&obj.key)) else {
            continue;
        };
        let group = groups.entry(parsed.cache_key).or_default();
        group.crate_name = parsed.crate_name;
        match parsed.kind {
            ObjectKind::Pack => group.pack = Some(obj.clone()),
            ObjectKind::Manifest => group.manifest = Some(obj.clone()),
        }
    }

    let mut candidates: Vec<Eviction> = groups
        .into_iter()
        .map(|(cache_key, group)| {
            let reason = if group.pack.is_some() && group.manifest.is_some() {
                EvictReason::Lfu
            } else {
                EvictReason::Orphan
            };
            let crate_name = group.crate_name.clone();
            let objects = group.objects();
            let last_modified = objects
                .iter()
                .map(|o| o.last_modified)
                .max()
                .unwrap_or(params.now);
            let usage = index.get(&cache_key).cloned().unwrap_or_default();
            Eviction {
                last_accessed: usage.last_accessed.unwrap_or(last_modified),
                hit_count: usage.hit_count,
                last_modified,
                cache_key,
                crate_name,
                objects,
                reason,
            }
        })
        .collect();
    candidates.sort_by(|a, b| {
        (
            a.reason != EvictReason::Orphan,
            a.hit_count,
            a.last_accessed,
            &a.cache_key,
        )
            .cmp(&(
                b.reason != EvictReason::Orphan,
                b.hit_count,
                b.last_accessed,
                &b.cache_key,
            ))
    });

    let cutoff = params.now - params.min_age;
    let mut remaining = total_bytes;
    for candidate in candidates {
        if remaining <= params.watermark_bytes {
            break;
        }
        if candidate.last_modified > cutoff {
            plan.skipped_young += candidate.objects.len() as u64;
            continue;
        }
        remaining -= candidate.bytes();
        plan.freed_bytes += candidate.bytes();
        plan.evictions.push(candidate);
    }
    plan
}

/// The planner receives keys as listed (prefix included); grouping only needs
/// the layout-relative part, so strip everything before the `v3/` marker.
fn key_without_prefix(key: &str) -> &str {
    match key.find("v3/") {
        Some(pos) if pos == 0 || key.as_bytes()[pos - 1] == b'/' => &key[pos..],
        _ => key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::UsageEntry;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn now() -> DateTime<Utc> {
        "2026-09-10T12:00:00Z".parse().expect("timestamp")
    }

    fn hours_ago(h: i64) -> DateTime<Utc> {
        now() - Duration::hours(h)
    }

    fn key(n: u8) -> String {
        format!("{n:02x}").repeat(32)
    }

    fn pack(n: u8, crate_name: &str, size: u64, age_hours: i64) -> RemoteObject {
        RemoteObject {
            key: format!("artifacts/v3/packs/{crate_name}/{}.tar.zst", key(n)),
            size,
            last_modified: hours_ago(age_hours),
        }
    }

    fn manifest(n: u8, crate_name: &str, size: u64, age_hours: i64) -> RemoteObject {
        RemoteObject {
            key: format!("artifacts/v3/manifests/{crate_name}/{}.json", key(n)),
            size,
            last_modified: hours_ago(age_hours),
        }
    }

    fn usage(entries: &[(u8, i64, i64)]) -> UsageMap {
        entries
            .iter()
            .map(|&(n, hits, accessed_hours_ago)| {
                (
                    key(n),
                    UsageEntry {
                        hit_count: hits,
                        last_accessed: Some(hours_ago(accessed_hours_ago)),
                    },
                )
            })
            .collect()
    }

    /// max 10 bytes, watermark 8 bytes, min_age 24h.
    fn params() -> GcParams {
        GcParams {
            max_bytes: 10,
            watermark_bytes: 8,
            min_age: Duration::hours(24),
            now: now(),
        }
    }

    fn evicted_keys(plan: &GcPlan) -> Vec<&str> {
        plan.evictions
            .iter()
            .map(|e| e.cache_key.as_str())
            .collect()
    }

    #[test]
    fn params_from_store_config() {
        let p = GcParams::from_store_config(
            &rbs_config::Store {
                max_size_gib: 40,
                low_watermark_percent: 90,
                min_age_hours: 24,
            },
            now(),
        );
        assert_eq!(p.max_bytes, 40 * GIB);
        assert_eq!(p.watermark_bytes, 36 * GIB);
        assert_eq!(p.min_age, Duration::hours(24));
        // percent above 100 is clamped
        let p = GcParams::from_store_config(
            &rbs_config::Store {
                max_size_gib: 1,
                low_watermark_percent: 150,
                min_age_hours: 0,
            },
            now(),
        );
        assert_eq!(p.watermark_bytes, p.max_bytes);
    }

    #[test]
    fn parse_object_key_accepts_layout_and_rejects_noise() {
        let k = key(1);
        let parsed = parse_object_key(
            "artifacts",
            &format!("artifacts/v3/packs/serde/{k}.tar.zst"),
        )
        .expect("pack");
        assert_eq!(parsed.kind, ObjectKind::Pack);
        assert_eq!(parsed.crate_name, "serde");
        assert_eq!(parsed.cache_key, k);
        let parsed =
            parse_object_key("", &format!("v3/manifests/serde/{k}.json")).expect("manifest");
        assert_eq!(parsed.kind, ObjectKind::Manifest);

        for bad in [
            format!("other/v3/packs/serde/{k}.tar.zst"), // wrong prefix
            format!("artifacts/v2/packs/serde/{k}.tar.zst"), // wrong version
            format!("artifacts/v3/blobs/serde/{k}.tar.zst"), // wrong kind
            format!("artifacts/v3/packs/serde/{k}.json"), // wrong extension for kind
            format!("artifacts/v3/packs/serde/deep/{k}.tar.zst"), // extra segment
            format!("artifacts/v3/packs/serde/{}.tar.zst", "Z".repeat(64)), // not hex
            format!("artifacts/v3/packs/serde/{}.tar.zst", "a".repeat(63)), // not 64 chars
            "artifacts/v3/packs".to_string(),
        ] {
            assert_eq!(parse_object_key("artifacts", &bad), None, "{bad}");
        }
    }

    #[test]
    fn under_cap_is_a_noop() {
        let objects = vec![pack(1, "a", 4, 48), manifest(1, "a", 1, 48)];
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert_eq!(plan.scanned, 2);
        assert_eq!(plan.total_bytes, 5);
        assert!(plan.evictions.is_empty());
        assert_eq!(plan.freed_bytes, 0);
        assert_eq!(plan.skipped_young, 0);
    }

    #[test]
    fn exactly_at_cap_is_a_noop() {
        let objects = vec![pack(1, "a", 10, 48)];
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert!(plan.evictions.is_empty());
    }

    #[test]
    fn over_cap_evicts_lowest_hit_count_first() {
        let objects = vec![
            pack(1, "a", 5, 48),
            manifest(1, "a", 1, 48),
            pack(2, "b", 5, 48),
            manifest(2, "b", 1, 48),
        ];
        // key 1 hot, key 2 cold; total 12 > 10, watermark 8.
        let index = usage(&[(1, 100, 1), (2, 1, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(2)]);
        assert_eq!(plan.freed_bytes, 6);
        assert_eq!(plan.evictions[0].reason, EvictReason::Lfu);
    }

    #[test]
    fn last_accessed_breaks_hit_count_ties() {
        let objects = vec![
            pack(1, "a", 6, 48),
            manifest(1, "a", 1, 48),
            pack(2, "b", 6, 48),
            manifest(2, "b", 1, 48),
        ];
        // same hits; key 1 accessed longer ago → evicted first.
        let index = usage(&[(1, 5, 40), (2, 5, 2)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(1)]);
    }

    #[test]
    fn keys_absent_from_index_rank_as_zero_hits() {
        let objects = vec![
            pack(1, "a", 6, 48),
            manifest(1, "a", 1, 48),
            pack(2, "b", 6, 48),
            manifest(2, "b", 1, 48),
        ];
        // key 2 unknown to the index; key 1 has a single hit → key 2 goes first.
        let index = usage(&[(1, 1, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(2)]);
        // unknown keys use S3 last_modified as last_accessed
        assert_eq!(plan.evictions[0].last_accessed, hours_ago(48));
    }

    #[test]
    fn min_age_guard_protects_young_objects_and_counts_them() {
        let objects = vec![
            pack(1, "a", 6, 2), // young: 2h < 24h
            manifest(1, "a", 1, 2),
            pack(2, "b", 6, 48),
            manifest(2, "b", 1, 48),
        ];
        // key 1 would be evicted first (0 hits vs 5) but is too young.
        let index = usage(&[(2, 5, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(2)]);
        assert_eq!(plan.skipped_young, 2);
    }

    #[test]
    fn pair_with_one_young_object_is_protected() {
        let objects = vec![
            pack(1, "a", 6, 48),
            manifest(1, "a", 1, 2), // freshly rewritten manifest
            pack(2, "b", 6, 48),
            manifest(2, "b", 1, 48),
        ];
        let index = usage(&[(2, 5, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(2)]);
        assert_eq!(plan.skipped_young, 2);
    }

    #[test]
    fn everything_young_evicts_nothing() {
        let objects = vec![
            pack(1, "a", 6, 1),
            manifest(1, "a", 1, 1),
            pack(2, "b", 6, 1),
            manifest(2, "b", 1, 1),
        ];
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert!(plan.evictions.is_empty());
        assert_eq!(plan.skipped_young, 4);
    }

    #[test]
    fn stops_at_the_watermark() {
        let objects = vec![
            pack(1, "a", 3, 48),
            pack(2, "b", 3, 48),
            manifest(2, "b", 1, 48),
            pack(3, "c", 3, 48),
            manifest(3, "c", 1, 48),
        ];
        // total 11 > 10; evicting the orphan (key 1, 3 bytes) reaches 8 ≤ watermark.
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert_eq!(evicted_keys(&plan), vec![key(1)]);
        assert_eq!(plan.freed_bytes, 3);
        assert_eq!(plan.skipped_young, 0, "nothing counted after target met");
    }

    #[test]
    fn orphans_evict_before_pairs_even_with_hits() {
        let objects = vec![
            manifest(1, "a", 3, 48), // orphan manifest with recorded hits
            pack(2, "b", 7, 48),
            manifest(2, "b", 1, 48),
        ];
        let index = usage(&[(1, 50, 1), (2, 0, 40)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(1)]);
        assert_eq!(plan.evictions[0].reason, EvictReason::Orphan);
    }

    #[test]
    fn young_orphans_are_protected() {
        let objects = vec![
            manifest(1, "a", 3, 2), // orphan but young
            pack(2, "b", 8, 48),
            manifest(2, "b", 1, 48),
        ];
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert_eq!(evicted_keys(&plan), vec![key(2)]);
        assert_eq!(plan.skipped_young, 1);
    }

    #[test]
    fn pairs_are_evicted_whole() {
        let objects = vec![
            pack(1, "a", 9, 48),
            manifest(1, "a", 2, 48),
            pack(2, "b", 1, 48),
            manifest(2, "b", 1, 48),
        ];
        let index = usage(&[(1, 0, 40), (2, 5, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(1)]);
        let evicted: Vec<&str> = plan.evictions[0]
            .objects
            .iter()
            .map(|o| o.key.as_str())
            .collect();
        assert_eq!(evicted.len(), 2, "pack and manifest both go");
        assert!(evicted.iter().any(|k| k.contains("packs")));
        assert!(evicted.iter().any(|k| k.contains("manifests")));
        assert_eq!(plan.freed_bytes, 11);
    }

    #[test]
    fn ties_fall_back_to_cache_key_order_deterministically() {
        let objects = vec![
            pack(2, "b", 6, 48),
            manifest(2, "b", 1, 48),
            pack(1, "a", 6, 48),
            manifest(1, "a", 1, 48),
        ];
        let index = usage(&[(1, 0, 48), (2, 0, 48)]);
        let a = plan(&objects, &index, &params());
        let mut reversed = objects.clone();
        reversed.reverse();
        let b = plan(&reversed, &index, &params());
        assert_eq!(a.evictions, b.evictions, "input order must not matter");
        assert_eq!(evicted_keys(&a), vec![key(1)]);
    }

    #[test]
    fn unrecognized_keys_count_toward_totals_but_are_never_evicted() {
        let objects = vec![
            RemoteObject {
                key: "artifacts/v3/other/stuff.bin".into(),
                size: 9,
                last_modified: hours_ago(999),
            },
            pack(1, "a", 6, 48),
            manifest(1, "a", 1, 48),
        ];
        let plan = plan(&objects, &UsageMap::new(), &params());
        assert_eq!(plan.scanned, 3);
        assert_eq!(plan.total_bytes, 16);
        // only the recognized pair can go; the plan never touches stuff.bin
        assert_eq!(evicted_keys(&plan), vec![key(1)]);
        assert!(
            plan.evictions
                .iter()
                .flat_map(|e| &e.objects)
                .all(|o| !o.key.contains("stuff.bin"))
        );
    }

    #[test]
    fn evicts_multiple_groups_until_watermark() {
        let objects = vec![
            pack(1, "a", 4, 48),
            manifest(1, "a", 1, 48),
            pack(2, "b", 4, 48),
            manifest(2, "b", 1, 48),
            pack(3, "c", 4, 48),
            manifest(3, "c", 1, 48),
        ];
        // total 15; need ≤ 8 → evict two lowest-hit groups (keys 3 then 1).
        let index = usage(&[(1, 2, 1), (2, 9, 1), (3, 1, 1)]);
        let plan = plan(&objects, &index, &params());
        assert_eq!(evicted_keys(&plan), vec![key(3), key(1)]);
        assert_eq!(plan.freed_bytes, 10);
    }
}
