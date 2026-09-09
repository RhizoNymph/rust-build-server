# proto — shared protocol & domain types

## Scope
Serde types and framing shared by client and server. No I/O beyond
encode/decode helpers. Non-scope: transport, scheduling, config loading.

## Files
- `crates/rbs-proto/src/lib.rs` — re-exports.
- `crates/rbs-proto/src/job.rs` — `JobRequest`, `JobEvent`, `JobId`, `Priority`, `ExitStatus`.
- `crates/rbs-proto/src/toolchain.rs` — `ToolchainFingerprint`.
- `crates/rbs-proto/src/status.rs` — `ServerStatus`, `Capacity`.
- `crates/rbs-proto/src/frame.rs` — newline-delimited JSON framing (`encode`, `Decoder`), `PROTOCOL_VERSION`.

## Types
```rust
pub const PROTOCOL_VERSION: u32 = 1;

pub enum ClientMessage { Hello { version: u32 }, Status, Submit(JobRequest), Cancel(JobId) }
pub enum ServerMessage { Hello { version: u32, server: ServerIdentity }, Status(ServerStatus), Event(JobEvent), Error(ProtoError) }

pub struct JobRequest {
    pub cwd: PathBuf,                 // absolute, mirrored path
    pub argv: Vec<String>,            // ["cargo", "build", "--release"]
    pub env: BTreeMap<String, String>,// allowlisted env only
    pub toolchain: ToolchainFingerprint,
    pub priority: Priority,           // Interactive | Agent | Background
    pub client: ClientIdentity,       // hostname, pid, label (agent name)
    pub tty: bool,                    // request colour / progress
}
pub enum JobEvent { Queued { id, position }, Started { id }, Stdout { id, bytes }, Stderr { id, bytes }, Exited { id, status: ExitStatus }, Rejected { id, reason: RejectReason } }
pub enum ExitStatus { Code(i32), Signal(i32) }
pub enum RejectReason { ToolchainMismatch { server: ToolchainFingerprint, client: ToolchainFingerprint }, Saturated, VersionMismatch, Internal(String) }

pub struct ToolchainFingerprint { pub rustc_commit: String, pub rustc_version: String, pub host: String, pub cargo_version: String, pub toolchain_name: String }

pub struct ServerStatus { pub identity: ServerIdentity, pub capacity: Capacity, pub queued: usize, pub running: usize, pub uptime_secs: u64 }
pub struct Capacity { pub tokens_total: u32, pub tokens_free: u32, pub mem_available_bytes: u64, pub load1: f32, pub accepting: bool }
```

## Invariants
- Every frame is one JSON object terminated by `\n`; no embedded newlines.
- `Hello` is the first message both directions; version mismatch → `Error` then close.
- `JobEvent` ordering per job: `Queued?` → `Started` → (`Stdout`|`Stderr`)* → `Exited`, or `Rejected` terminal.
- Types are `#[non_exhaustive]`-free and `#[serde(deny_unknown_fields)]` so drift is caught in tests.
