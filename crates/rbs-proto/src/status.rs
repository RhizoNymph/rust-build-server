use serde::{Deserialize, Serialize};

/// Identity of a server process, sent in `Hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerIdentity {
    pub hostname: String,
    /// rbs crate version of the server binary.
    pub version: String,
}

/// Point-in-time resource snapshot used for admission and client-side backend choice.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capacity {
    /// Size of the jobserver token pool (compile slots shared by all jobs).
    pub tokens_total: u32,
    /// Tokens not currently held by any rustc/cargo.
    pub tokens_free: u32,
    /// `MemAvailable` from `/proc/meminfo`.
    pub mem_available_bytes: u64,
    /// 1-minute load average.
    pub load1: f32,
    /// Whether the server would accept a new job right now without queueing.
    pub accepting: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerStatus {
    pub identity: ServerIdentity,
    pub capacity: Capacity,
    pub queued: usize,
    pub running: usize,
    pub uptime_secs: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        let st = ServerStatus {
            identity: ServerIdentity {
                hostname: "node0".into(),
                version: "0.1.0".into(),
            },
            capacity: Capacity {
                tokens_total: 28,
                tokens_free: 10,
                mem_available_bytes: 1 << 36,
                load1: 3.5,
                accepting: true,
            },
            queued: 1,
            running: 2,
            uptime_secs: 99,
        };
        let json = serde_json::to_string(&st).expect("serialize");
        let back: ServerStatus = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, st);
    }
}
