//! Size cap + LFU eviction for the shared kache S3 store.
//! See `docs/features/store-gc.md`.
//!
//! Everything decision-making is pure ([`planner`]); I/O lives at the edges:
//! kache config + AWS credentials parsing ([`kache_cfg`], [`creds`]), the
//! read-only sqlite usage index ([`index`]), and S3 listing/deletion ([`gc`],
//! [`s3`]). [`run_gc`] is the `anyhow` boundary the CLI calls.

pub mod creds;
pub mod gc;
pub mod index;
pub mod kache_cfg;
pub mod planner;
pub mod s3;

use std::path::PathBuf;

use anyhow::Context;
use chrono::Utc;
use tracing::info;

pub use gc::{GcError, execute_plan, format_plan, list_objects};
pub use planner::{EvictReason, Eviction, GcParams, GcPlan, RemoteObject, plan};

/// Options for one GC run.
#[derive(Debug, Clone, Default)]
pub struct GcOpts {
    /// Print the eviction plan without deleting anything.
    pub dry_run: bool,
    /// Override `[store] max_size_gib` from the rbs config.
    pub max_size_gib_override: Option<u32>,
}

/// What a GC run saw and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcReport {
    /// Objects in the remote listing.
    pub scanned: u64,
    /// Total listed bytes before eviction.
    pub total_bytes: u64,
    /// Objects deleted (or that would be deleted, for a dry run).
    pub evicted: u64,
    /// Bytes freed by the evictions.
    pub freed_bytes: u64,
    /// Objects protected by `min_age_hours` while still over target.
    pub skipped_young: u64,
}

impl std::fmt::Display for GcReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MIB: f64 = 1024.0 * 1024.0;
        write!(
            f,
            "scanned {} objects ({:.1} MiB); evicted {} ({:.1} MiB freed); {} skipped (too young)",
            self.scanned,
            self.total_bytes as f64 / MIB,
            self.evicted,
            self.freed_bytes as f64 / MIB,
            self.skipped_young,
        )
    }
}

/// Filesystem locations `run_gc` reads, resolved from the environment.
struct GcPaths {
    kache_config: PathBuf,
    aws_credentials: PathBuf,
    index_db: PathBuf,
}

impl GcPaths {
    fn from_env() -> anyhow::Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("could not determine home directory ($HOME unset)")?;
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".cache"));
        Ok(Self {
            kache_config: config_dir.join("kache/config.toml"),
            aws_credentials: home.join(".aws/credentials"),
            index_db: cache_dir.join("kache/index.db"),
        })
    }
}

/// Run one GC pass against the shared store. Reads the rbs `[store]` config,
/// kache's remote settings and local usage index, plans, then deletes (unless
/// `dry_run`).
pub async fn run_gc(opts: GcOpts) -> anyhow::Result<GcReport> {
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let mut store_cfg = rbs_config::load(&cwd).context("loading rbs config")?.store;
    if let Some(max) = opts.max_size_gib_override {
        store_cfg.max_size_gib = max;
    }
    if store_cfg.max_size_gib == 0 {
        info!("store gc disabled (max_size_gib = 0)");
        return Ok(GcReport::default());
    }

    let paths = GcPaths::from_env()?;
    let remote = kache_cfg::load_kache_config(&paths.kache_config)?;
    let credentials_file = match std::fs::read_to_string(&paths.aws_credentials) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(e).context(format!("reading {}", paths.aws_credentials.display()));
        }
    };
    let credentials = creds::resolve_credentials(
        |k| std::env::var(k).ok(),
        credentials_file.as_deref(),
        &paths.aws_credentials,
        remote.profile.as_deref().unwrap_or("default"),
    )?;
    let store = s3::build_s3(&remote, &credentials)?;

    info!(
        bucket = remote.bucket,
        prefix = remote.prefix.as_deref().unwrap_or(""),
        max_size_gib = store_cfg.max_size_gib,
        dry_run = opts.dry_run,
        "store gc starting"
    );
    let objects = gc::list_objects(&store, remote.prefix.as_deref()).await?;
    let index = index::load_index_or_empty(&paths.index_db)?;
    let now = Utc::now();
    let params = GcParams::from_store_config(&store_cfg, now);
    let plan = planner::plan(&objects, &index, &params);
    let report = gc::execute_plan(&store, &plan, opts.dry_run, now).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_display_is_stable() {
        let r = GcReport {
            scanned: 10,
            total_bytes: 2 * 1024 * 1024,
            evicted: 4,
            freed_bytes: 1024 * 1024,
            skipped_young: 2,
        };
        assert_eq!(
            r.to_string(),
            "scanned 10 objects (2.0 MiB); evicted 4 (1.0 MiB freed); 2 skipped (too young)"
        );
    }
}
