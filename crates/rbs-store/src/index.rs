//! Read-only view of kache's local sqlite index (`~/.cache/kache/index.db`).
//!
//! The GC runs on the build-server host where every remote build executes, so
//! this index's hit counts are a global frequency view of the shared store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, Utc};
use rusqlite::OpenFlags;
use thiserror::Error;
use tracing::warn;

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("failed to open kache index {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("failed to read kache index {path}: {source}")]
    Query {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

/// Usage stats for one cache key, from the `entries` table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UsageEntry {
    pub hit_count: i64,
    pub last_accessed: Option<DateTime<Utc>>,
}

/// Map from 64-hex cache key to its usage stats.
pub type UsageMap = HashMap<String, UsageEntry>;

/// Parse the TEXT datetime formats sqlite/kache produce. Unparseable → `None`.
pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    None
}

/// Load the index read-only. The caller decides what a missing file means.
pub fn load_index(path: &Path) -> Result<UsageMap, IndexError> {
    let open_err = |source| IndexError::Open {
        path: path.to_path_buf(),
        source,
    };
    let query_err = |source| IndexError::Query {
        path: path.to_path_buf(),
        source,
    };
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let uri = format!("file:{}?mode=ro", path.display());
    let conn = rusqlite::Connection::open_with_flags(&uri, flags).map_err(open_err)?;
    let mut stmt = conn
        .prepare("SELECT cache_key, hit_count, last_accessed FROM entries")
        .map_err(query_err)?;
    let rows = stmt
        .query_map([], |row| {
            let cache_key: String = row.get(0)?;
            let hit_count: i64 = row.get(1)?;
            let last_accessed: Option<String> = row.get(2)?;
            Ok((cache_key, hit_count, last_accessed))
        })
        .map_err(query_err)?;
    let mut map = UsageMap::new();
    for row in rows {
        let (cache_key, hit_count, last_accessed) = row.map_err(query_err)?;
        let last_accessed = last_accessed.as_deref().and_then(parse_datetime);
        map.insert(
            cache_key,
            UsageEntry {
                hit_count,
                last_accessed,
            },
        );
    }
    Ok(map)
}

/// Load the index, treating a missing file as an empty map (with a warning):
/// GC then ranks every object as never-hit, which is safe but less precise.
pub fn load_index_or_empty(path: &Path) -> Result<UsageMap, IndexError> {
    if !path.exists() {
        warn!(path = %path.display(), "kache index missing; treating all objects as never hit");
        return Ok(UsageMap::new());
    }
    load_index(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SCHEMA: &str = "CREATE TABLE entries (
        cache_key TEXT PRIMARY KEY,
        crate_name TEXT,
        size INTEGER,
        created_at TEXT,
        last_accessed TEXT,
        hit_count INTEGER,
        extra_column TEXT
    )";

    pub(crate) fn seed_db(path: &Path, rows: &[(&str, &str, i64)]) {
        let conn = rusqlite::Connection::open(path).expect("open");
        conn.execute(SCHEMA, []).expect("schema");
        for (key, last_accessed, hits) in rows {
            conn.execute(
                "INSERT INTO entries (cache_key, crate_name, size, created_at, last_accessed, hit_count)
                 VALUES (?1, 'c', 1, ?2, ?2, ?3)",
                rusqlite::params![key, last_accessed, hits],
            )
            .expect("insert");
        }
    }

    #[test]
    fn reads_entries_read_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.db");
        seed_db(
            &db,
            &[
                ("a".repeat(64).leak(), "2026-09-01 10:00:00", 7),
                ("b".repeat(64).leak(), "2026-09-02T11:30:00", 0),
            ],
        );
        let map = load_index(&db).expect("load");
        assert_eq!(map.len(), 2);
        let a = &map[&"a".repeat(64)];
        assert_eq!(a.hit_count, 7);
        assert_eq!(
            a.last_accessed,
            parse_datetime("2026-09-01 10:00:00"),
            "space-separated datetime parses"
        );
        assert!(map[&"b".repeat(64)].last_accessed.is_some());
    }

    #[test]
    fn unparseable_last_accessed_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("index.db");
        seed_db(&db, &[("c".repeat(64).leak(), "not a date", 3)]);
        let map = load_index(&db).expect("load");
        assert_eq!(map[&"c".repeat(64)].hit_count, 3);
        assert_eq!(map[&"c".repeat(64)].last_accessed, None);
    }

    #[test]
    fn missing_file_yields_empty_map() {
        let dir = tempfile::tempdir().expect("tempdir");
        let map = load_index_or_empty(&dir.path().join("nope.db")).expect("empty");
        assert!(map.is_empty());
        assert!(load_index(&dir.path().join("nope.db")).is_err());
    }

    #[test]
    fn datetime_formats() {
        assert!(parse_datetime("2026-09-01 10:00:00").is_some());
        assert!(parse_datetime("2026-09-01 10:00:00.123").is_some());
        assert!(parse_datetime("2026-09-01T10:00:00").is_some());
        assert!(parse_datetime("2026-09-01T10:00:00Z").is_some());
        assert!(parse_datetime("2026-09-01T10:00:00+02:00").is_some());
        assert_eq!(parse_datetime("bogus"), None);
    }
}
