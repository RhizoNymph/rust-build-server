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
pub mod touch;

use std::path::PathBuf;

use anyhow::Context;
use chrono::Utc;
use tracing::{debug, info};

pub use gc::{GcError, execute_plan, format_plan, list_objects};
pub use planner::{EvictReason, Eviction, GcParams, GcPlan, RemoteObject, plan};
pub use touch::{TouchError, TouchPlan, parse_cargo_lock, plan_touch};

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
    /// Stranded `*.touch-tmp` objects removed (or that would be, for a dry run).
    pub junk_deleted: u64,
}

impl std::fmt::Display for GcReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MIB: f64 = 1024.0 * 1024.0;
        write!(
            f,
            "scanned {} objects ({:.1} MiB); evicted {} ({:.1} MiB freed); {} skipped (too young); {} junk removed",
            self.scanned,
            self.total_bytes as f64 / MIB,
            self.evicted,
            self.freed_bytes as f64 / MIB,
            self.skipped_young,
            self.junk_deleted,
        )
    }
}

/// Filesystem locations `run_gc` / `run_touch` read, resolved from the environment.
struct StorePaths {
    kache_config: PathBuf,
    aws_credentials: PathBuf,
    index_db: PathBuf,
    cache_dir: PathBuf,
}

impl StorePaths {
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
            cache_dir,
        })
    }
}

/// Load kache's remote settings, resolve credentials, and build the S3 client.
fn connect_store(
    paths: &StorePaths,
) -> anyhow::Result<(kache_cfg::RemoteStore, object_store::aws::AmazonS3)> {
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
    Ok((remote, store))
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

    let paths = StorePaths::from_env()?;
    let (remote, store) = connect_store(&paths)?;

    info!(
        bucket = remote.bucket,
        prefix = remote.prefix.as_deref().unwrap_or(""),
        max_size_gib = store_cfg.max_size_gib,
        dry_run = opts.dry_run,
        "store gc starting"
    );
    let objects = gc::list_objects(&store, remote.prefix.as_deref()).await?;
    let index = index::load_index_or_empty(&paths.index_db);
    let now = Utc::now();
    let params = GcParams::from_store_config(&store_cfg, now);
    let plan = planner::plan(&objects, &index, &params);
    let report = gc::execute_plan(&store, &plan, opts.dry_run, now).await?;
    Ok(report)
}

/// Options for one store-touch run.
#[derive(Debug, Clone, Default)]
pub struct TouchOpts {
    /// Directory whose workspace to touch; defaults to the cwd. The actual
    /// root is always resolved via `cargo locate-project --workspace`.
    pub workspace: Option<PathBuf>,
    /// Run even if the throttle stamp is fresh.
    pub force: bool,
}

/// What a store-touch run saw and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TouchReport {
    /// The throttle stamp was fresh; nothing was listed or touched.
    pub throttled: bool,
    /// Crates read from the workspace `Cargo.lock`.
    pub crates: u64,
    /// Objects listed across the crates' store prefixes.
    pub listed: u64,
    /// Objects rewritten (last_modified refreshed).
    pub touched: u64,
    /// Objects already fresher than `touch_after_hours`.
    pub skipped_fresh: u64,
    /// Objects whose touch failed (warned and skipped).
    pub failures: u64,
}

impl std::fmt::Display for TouchReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.throttled {
            return write!(f, "throttled (stamp is fresh); nothing touched");
        }
        write!(
            f,
            "touched {} of {} objects across {} crates; {} fresh; {} failures",
            self.touched, self.listed, self.crates, self.skipped_fresh, self.failures,
        )
    }
}

/// Touch this workspace's store objects so their S3 last-modified reflects
/// use on this machine. Throttled per workspace by a stamp file; the stamp is
/// only refreshed after a run with no failures.
pub async fn run_touch(opts: TouchOpts) -> anyhow::Result<TouchReport> {
    let dir = match opts.workspace {
        Some(d) => d,
        None => std::env::current_dir().context("cannot determine cwd")?,
    };
    let store_cfg = rbs_config::load(&dir).context("loading rbs config")?.store;
    if !store_cfg.touch {
        info!("store touch disabled ([store] touch = false)");
        return Ok(TouchReport::default());
    }
    let root = rbs_sync::workspace_root(&dir)
        .await
        .context("resolving workspace root")?;
    let paths = StorePaths::from_env()?;
    let stamp = touch::stamp_path(&paths.cache_dir, &root);
    let max_age = std::time::Duration::from_secs(u64::from(store_cfg.touch_after_hours) * 3600);
    let now_sys = std::time::SystemTime::now();
    if !opts.force && touch::stamp_fresh_on_disk(&stamp, now_sys, max_age) {
        debug!(stamp = %stamp.display(), "touch stamp is fresh; skipping");
        return Ok(TouchReport {
            throttled: true,
            ..TouchReport::default()
        });
    }

    let lock_path = root.join("Cargo.lock");
    let lock_text = std::fs::read_to_string(&lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let crates = touch::parse_cargo_lock(&lock_text)?;

    let (remote, store) = connect_store(&paths)?;
    info!(
        bucket = remote.bucket,
        prefix = remote.prefix.as_deref().unwrap_or(""),
        root = %root.display(),
        crates = crates.len(),
        force = opts.force,
        "store touch starting"
    );
    let prefixes: Vec<String> = crates
        .iter()
        .flat_map(|c| touch::crate_prefixes(remote.prefix.as_deref(), c))
        .collect();
    let objects = touch::list_prefixes(&store, &prefixes).await?;
    let cutoff = Utc::now() - chrono::Duration::hours(i64::from(store_cfg.touch_after_hours));
    let plan = touch::plan_touch(&objects, cutoff);
    let (touched, failures) = touch::execute_touch(&touch::StoreOps(&store), &plan.touch).await;

    let report = TouchReport {
        throttled: false,
        crates: crates.len() as u64,
        listed: objects.len() as u64,
        touched,
        skipped_fresh: plan.skipped_fresh,
        failures,
    };
    if failures == 0 {
        touch::write_stamp(&stamp).with_context(|| format!("writing stamp {}", stamp.display()))?;
    }
    info!(
        crates = report.crates,
        listed = report.listed,
        touched = report.touched,
        skipped_fresh = report.skipped_fresh,
        failures = report.failures,
        "store touch complete"
    );
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
            junk_deleted: 1,
        };
        assert_eq!(
            r.to_string(),
            "scanned 10 objects (2.0 MiB); evicted 4 (1.0 MiB freed); 2 skipped (too young); 1 junk removed"
        );
    }

    #[test]
    fn touch_report_display_is_stable() {
        let r = TouchReport {
            throttled: false,
            crates: 3,
            listed: 12,
            touched: 5,
            skipped_fresh: 6,
            failures: 1,
        };
        assert_eq!(
            r.to_string(),
            "touched 5 of 12 objects across 3 crates; 6 fresh; 1 failures"
        );
        let throttled = TouchReport {
            throttled: true,
            ..TouchReport::default()
        };
        assert_eq!(
            throttled.to_string(),
            "throttled (stamp is fresh); nothing touched"
        );
    }
}
