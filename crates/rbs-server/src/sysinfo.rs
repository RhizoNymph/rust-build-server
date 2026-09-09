//! `/proc/meminfo` and `/proc/loadavg` readers with pure, unit-tested parsers.

use std::path::Path;

use tracing::warn;

/// Extract `MemAvailable` (in bytes) from `/proc/meminfo` text.
pub fn parse_mem_available(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemAvailable:") else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let value: u64 = it.next()?.parse().ok()?;
        let unit = it.next().unwrap_or("kB");
        let mult = match unit {
            "kB" | "KB" | "kiB" | "KiB" => 1024,
            "mB" | "MB" | "MiB" => 1024 * 1024,
            "B" => 1,
            _ => return None,
        };
        return value.checked_mul(mult);
    }
    None
}

/// Extract the 1-minute load average from `/proc/loadavg` text.
pub fn parse_load1(text: &str) -> Option<f32> {
    text.split_whitespace().next()?.parse().ok()
}

/// Point-in-time host resource snapshot used by the scheduler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub mem_available_bytes: u64,
    pub load1: f32,
}

impl Snapshot {
    /// A snapshot for hosts where `/proc` is unreadable: treat memory as
    /// unlimited so admission degrades to job-count only.
    pub const UNKNOWN: Snapshot = Snapshot {
        mem_available_bytes: u64::MAX,
        load1: 0.0,
    };
}

async fn read_proc(path: &Path) -> Option<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read proc file");
            None
        }
    }
}

/// Read the live snapshot; falls back to [`Snapshot::UNKNOWN`] fields on error.
pub async fn snapshot() -> Snapshot {
    let mem = read_proc(Path::new("/proc/meminfo"))
        .await
        .and_then(|t| parse_mem_available(&t))
        .unwrap_or(Snapshot::UNKNOWN.mem_available_bytes);
    let load1 = read_proc(Path::new("/proc/loadavg"))
        .await
        .and_then(|t| parse_load1(&t))
        .unwrap_or(Snapshot::UNKNOWN.load1);
    Snapshot {
        mem_available_bytes: mem,
        load1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MEMINFO: &str = "MemTotal:       65536000 kB\nMemFree:        12345678 kB\nMemAvailable:   45678901 kB\nBuffers:          123456 kB\n";

    #[test]
    fn parses_mem_available_in_kib() {
        assert_eq!(parse_mem_available(MEMINFO), Some(45_678_901 * 1024));
    }

    #[test]
    fn mem_available_missing_or_malformed() {
        assert_eq!(parse_mem_available("MemTotal: 1 kB\n"), None);
        assert_eq!(parse_mem_available("MemAvailable: lots kB\n"), None);
        assert_eq!(parse_mem_available("MemAvailable: 5 furlongs\n"), None);
        assert_eq!(parse_mem_available(""), None);
    }

    #[test]
    fn mem_available_other_units() {
        assert_eq!(parse_mem_available("MemAvailable: 7 B\n"), Some(7));
        assert_eq!(
            parse_mem_available("MemAvailable: 3 MB\n"),
            Some(3 * 1024 * 1024)
        );
        assert_eq!(parse_mem_available("MemAvailable: 9\n"), Some(9 * 1024));
    }

    #[test]
    fn parses_load1() {
        assert_eq!(parse_load1("3.52 2.10 1.00 3/2345 12345\n"), Some(3.52));
        assert_eq!(parse_load1("0.00 0.00 0.00 1/1 1\n"), Some(0.0));
        assert_eq!(parse_load1(""), None);
        assert_eq!(parse_load1("x y z"), None);
    }

    #[tokio::test]
    async fn live_snapshot_is_plausible() {
        let s = snapshot().await;
        assert!(s.load1 >= 0.0);
        assert!(s.mem_available_bytes > 0);
    }
}
