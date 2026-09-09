//! Shared protocol and domain types for rbs.
//!
//! Framing is newline-delimited JSON (see [`frame`]). Every message type is
//! `deny_unknown_fields` so client/server drift is caught by tests rather than
//! silently ignored.

pub mod frame;
pub mod job;
pub mod status;
pub mod toolchain;

pub use frame::{Decoder, FrameError, PROTOCOL_VERSION, encode};
pub use job::{ClientIdentity, ExitStatus, JobEvent, JobId, JobRequest, Priority, RejectReason};
pub use status::{Capacity, ServerIdentity, ServerStatus};
pub use toolchain::ToolchainFingerprint;

use serde::{Deserialize, Serialize};

/// Messages sent from client to server.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClientMessage {
    Hello { version: u32 },
    Status,
    Submit(JobRequest),
    Cancel { id: JobId },
}

/// Messages sent from server to client.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServerMessage {
    Hello {
        version: u32,
        server: ServerIdentity,
    },
    Status(ServerStatus),
    Event(JobEvent),
    Error(ProtoError),
}

/// Protocol-level error reported by the server (not a job failure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct ProtoError {
    pub code: ProtoErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtoErrorCode {
    VersionMismatch,
    UnexpectedMessage,
    Internal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn sample_request() -> JobRequest {
        JobRequest {
            cwd: PathBuf::from("/home/user/Code/x"),
            argv: vec!["cargo".into(), "build".into()],
            env: BTreeMap::from([("RUSTFLAGS".to_string(), "-Cdebuginfo=0".to_string())]),
            toolchain: ToolchainFingerprint {
                rustc_commit: "59807616e".into(),
                rustc_version: "1.95.0".into(),
                host: "x86_64-unknown-linux-gnu".into(),
                cargo_version: "cargo 1.95.0".into(),
                toolchain_name: "1.95.0-x86_64-unknown-linux-gnu".into(),
            },
            priority: Priority::Agent,
            client: ClientIdentity {
                hostname: "framework".into(),
                pid: 42,
                label: None,
            },
            tty: false,
        }
    }

    #[test]
    fn client_message_round_trips() {
        for msg in [
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            ClientMessage::Status,
            ClientMessage::Submit(sample_request()),
            ClientMessage::Cancel { id: JobId(7) },
        ] {
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: ClientMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn server_message_round_trips() {
        let fp = sample_request().toolchain;
        for msg in [
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                server: ServerIdentity {
                    hostname: "node0".into(),
                    version: "0.1.0".into(),
                },
            },
            ServerMessage::Event(JobEvent::Rejected {
                id: JobId(1),
                reason: RejectReason::ToolchainMismatch {
                    server: fp.clone(),
                    client: fp,
                },
            }),
            ServerMessage::Error(ProtoError {
                code: ProtoErrorCode::VersionMismatch,
                message: "x".into(),
            }),
        ] {
            let json = serde_json::to_string(&msg).expect("serialize");
            let back: ServerMessage = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, msg);
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let json = r#"{"type":"hello","version":1,"extra":true}"#;
        assert!(serde_json::from_str::<ClientMessage>(json).is_err());
    }

    #[test]
    fn tagged_wire_shape_is_stable() {
        let json = serde_json::to_string(&ClientMessage::Status).expect("serialize");
        assert_eq!(json, r#"{"type":"status"}"#);
    }
}
