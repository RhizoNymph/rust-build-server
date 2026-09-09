use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::toolchain::ToolchainFingerprint;

/// Server-assigned job identifier, unique per server process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub u64);

/// Scheduling priority. Ordering: `Interactive` > `Agent` > `Background`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Interactive,
    Agent,
    Background,
}

impl Priority {
    /// Higher is more urgent; used by the scheduler's ordering.
    pub fn rank(self) -> u8 {
        match self {
            Priority::Interactive => 2,
            Priority::Agent => 1,
            Priority::Background => 0,
        }
    }

    /// systemd `CPUWeight=` value for the job's scope.
    pub fn cpu_weight(self) -> u32 {
        match self {
            Priority::Interactive => 200,
            Priority::Agent => 100,
            Priority::Background => 50,
        }
    }
}

impl std::str::FromStr for Priority {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "interactive" => Ok(Priority::Interactive),
            "agent" => Ok(Priority::Agent),
            "background" => Ok(Priority::Background),
            other => Err(format!("unknown priority `{other}`")),
        }
    }
}

/// Who submitted a job; informational, shown in `rbs status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub hostname: String,
    pub pid: u32,
    /// Free-form label, e.g. an agent name from `RBS_LABEL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A request to run a command (normally `cargo …`) on the server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobRequest {
    /// Absolute working directory; identical on client and server (mirrored layout).
    pub cwd: PathBuf,
    /// Full argv including the program name, e.g. `["cargo", "build", "--release"]`.
    pub argv: Vec<String>,
    /// Environment to set for the job. The client has already applied its allowlist.
    pub env: BTreeMap<String, String>,
    /// Client's toolchain for `cwd`; the server verifies it matches its own.
    pub toolchain: ToolchainFingerprint,
    pub priority: Priority,
    pub client: ClientIdentity,
    /// Whether the client's stdout is a terminal (server may allocate a pty / enable colour).
    pub tty: bool,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExitStatus {
    Code(i32),
    Signal(i32),
}

impl ExitStatus {
    /// Exit code the client process should exit with.
    pub fn as_process_exit_code(self) -> i32 {
        match self {
            ExitStatus::Code(c) => c,
            ExitStatus::Signal(s) => 128 + s,
        }
    }

    pub fn success(self) -> bool {
        matches!(self, ExitStatus::Code(0))
    }
}

/// Why the server refused to run a job.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RejectReason {
    ToolchainMismatch {
        server: ToolchainFingerprint,
        client: ToolchainFingerprint,
    },
    Saturated,
    VersionMismatch,
    Internal(String),
}

/// Streamed lifecycle of a job. Order per job:
/// `Queued?` → `Started` → (`Stdout` | `Stderr`)* → `Exited`, or `Rejected` (terminal).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobEvent {
    Queued {
        id: JobId,
        position: usize,
    },
    Started {
        id: JobId,
    },
    Stdout {
        id: JobId,
        #[serde(with = "bytes_b64")]
        bytes: Vec<u8>,
    },
    Stderr {
        id: JobId,
        #[serde(with = "bytes_b64")]
        bytes: Vec<u8>,
    },
    Exited {
        id: JobId,
        status: ExitStatus,
    },
    Rejected {
        id: JobId,
        reason: RejectReason,
    },
}

impl JobEvent {
    pub fn id(&self) -> JobId {
        match self {
            JobEvent::Queued { id, .. }
            | JobEvent::Started { id }
            | JobEvent::Stdout { id, .. }
            | JobEvent::Stderr { id, .. }
            | JobEvent::Exited { id, .. }
            | JobEvent::Rejected { id, .. } => *id,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, JobEvent::Exited { .. } | JobEvent::Rejected { .. })
    }
}

/// Output bytes are arbitrary (may be invalid UTF-8, contain newlines); carry them as base64
/// so the newline-delimited framing stays intact. Hand-rolled to avoid a dependency.
mod bytes_b64 {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for chunk in input.chunks(3) {
            let b = [
                chunk[0],
                *chunk.get(1).unwrap_or(&0),
                *chunk.get(2).unwrap_or(&0),
            ];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(TABLE[(n >> 18) as usize & 63] as char);
            out.push(TABLE[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                TABLE[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                TABLE[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    pub fn decode(input: &str) -> Result<Vec<u8>, String> {
        fn val(c: u8) -> Result<u32, String> {
            match c {
                b'A'..=b'Z' => Ok(u32::from(c - b'A')),
                b'a'..=b'z' => Ok(u32::from(c - b'a') + 26),
                b'0'..=b'9' => Ok(u32::from(c - b'0') + 52),
                b'+' => Ok(62),
                b'/' => Ok(63),
                _ => Err(format!("invalid base64 byte {c:#x}")),
            }
        }
        let bytes = input.as_bytes();
        if !bytes.len().is_multiple_of(4) {
            return Err("base64 length not a multiple of 4".into());
        }
        let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
        for q in bytes.chunks(4) {
            let pad = q.iter().rev().take_while(|&&c| c == b'=').count();
            if pad > 2 {
                return Err("invalid base64 padding".into());
            }
            let mut n = 0u32;
            for (i, &c) in q.iter().enumerate() {
                let v = if c == b'=' {
                    if i < 4 - pad {
                        return Err("padding in the middle of a quantum".into());
                    }
                    0
                } else {
                    val(c)?
                };
                n = (n << 6) | v;
            }
            out.push((n >> 16) as u8);
            if pad < 2 {
                out.push((n >> 8) as u8);
            }
            if pad < 1 {
                out.push(n as u8);
            }
        }
        Ok(out)
    }

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        encode(bytes).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        decode(&s).map_err(serde::de::Error::custom)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_all_lengths() {
            for len in 0..64usize {
                let data: Vec<u8> = (0..len).map(|i| (i * 37 % 256) as u8).collect();
                assert_eq!(decode(&encode(&data)).expect("decode"), data, "len {len}");
            }
        }

        #[test]
        fn known_vectors() {
            assert_eq!(encode(b""), "");
            assert_eq!(encode(b"f"), "Zg==");
            assert_eq!(encode(b"fo"), "Zm8=");
            assert_eq!(encode(b"foo"), "Zm9v");
            assert_eq!(encode(b"foobar"), "Zm9vYmFy");
        }

        #[test]
        fn rejects_garbage() {
            assert!(decode("Zm9").is_err());
            assert!(decode("Zm=v").is_err());
            assert!(decode("!!!!").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_ordering_and_parse() {
        assert!(Priority::Interactive.rank() > Priority::Agent.rank());
        assert!(Priority::Agent.rank() > Priority::Background.rank());
        assert_eq!("agent".parse::<Priority>(), Ok(Priority::Agent));
        assert!("urgent".parse::<Priority>().is_err());
    }

    #[test]
    fn exit_status_mapping() {
        assert_eq!(ExitStatus::Code(3).as_process_exit_code(), 3);
        assert_eq!(ExitStatus::Signal(9).as_process_exit_code(), 137);
        assert!(ExitStatus::Code(0).success());
        assert!(!ExitStatus::Signal(15).success());
    }

    #[test]
    fn stdout_event_carries_raw_bytes_through_json() {
        let ev = JobEvent::Stdout {
            id: JobId(3),
            bytes: vec![0, 10, 13, 0xff, b'a'],
        };
        let json = serde_json::to_string(&ev).expect("serialize");
        assert!(!json.contains('\n'));
        let back: JobEvent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, ev);
        assert_eq!(back.id(), JobId(3));
        assert!(!back.is_terminal());
        assert!(
            JobEvent::Exited {
                id: JobId(3),
                status: ExitStatus::Code(0)
            }
            .is_terminal()
        );
    }
}
