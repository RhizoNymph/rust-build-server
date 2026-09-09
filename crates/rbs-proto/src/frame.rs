//! Newline-delimited JSON framing.
//!
//! One JSON object per line. Output bytes inside events are base64 so no
//! payload can contain a raw `\n`.

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Bumped on any incompatible wire change. Exchanged in `Hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Upper bound for a single frame; protects against a runaway peer.
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame exceeds {MAX_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("invalid JSON frame: {0}")]
    Json(#[from] serde_json::Error),
}

/// Serialize `msg` as one line (including the trailing newline).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, FrameError> {
    let mut v = serde_json::to_vec(msg)?;
    debug_assert!(
        !v.contains(&b'\n'),
        "serialized frame must not contain newlines"
    );
    v.push(b'\n');
    Ok(v)
}

/// Incremental line decoder. Feed bytes with [`Decoder::push`], drain frames with
/// [`Decoder::next_frame`].
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<(), FrameError> {
        if self.buf.len() + bytes.len() > MAX_FRAME_BYTES {
            return Err(FrameError::TooLarge);
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Returns the next complete frame, or `Ok(None)` if more bytes are needed.
    pub fn next_frame<T: DeserializeOwned>(&mut self) -> Result<Option<T>, FrameError> {
        loop {
            let Some(pos) = self.buf.iter().position(|&b| b == b'\n') else {
                return Ok(None);
            };
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = &line[..line.len() - 1];
            if line.iter().all(u8::is_ascii_whitespace) {
                continue; // tolerate blank lines / keepalives
            }
            return Ok(Some(serde_json::from_slice(line)?));
        }
    }

    /// Bytes buffered but not yet forming a complete frame.
    pub fn pending(&self) -> usize {
        self.buf.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientMessage;

    #[test]
    fn encode_appends_single_newline() {
        let bytes = encode(&ClientMessage::Status).expect("encode");
        assert_eq!(bytes, b"{\"type\":\"status\"}\n");
    }

    #[test]
    fn decoder_handles_split_and_batched_frames() {
        let mut d = Decoder::new();
        let a = encode(&ClientMessage::Status).expect("encode");
        let b = encode(&ClientMessage::Hello { version: 1 }).expect("encode");
        let mut all = a.clone();
        all.extend_from_slice(&b);
        // Feed in awkward chunks.
        d.push(&all[..3]).expect("push");
        assert!(d.next_frame::<ClientMessage>().expect("decode").is_none());
        d.push(&all[3..]).expect("push");
        assert_eq!(
            d.next_frame::<ClientMessage>().expect("decode"),
            Some(ClientMessage::Status)
        );
        assert_eq!(
            d.next_frame::<ClientMessage>().expect("decode"),
            Some(ClientMessage::Hello { version: 1 })
        );
        assert!(d.next_frame::<ClientMessage>().expect("decode").is_none());
        assert_eq!(d.pending(), 0);
    }

    #[test]
    fn decoder_skips_blank_lines_and_reports_bad_json() {
        let mut d = Decoder::new();
        d.push(b"\n  \n{\"type\":\"status\"}\nnot json\n")
            .expect("push");
        assert_eq!(
            d.next_frame::<ClientMessage>().expect("decode"),
            Some(ClientMessage::Status)
        );
        assert!(matches!(
            d.next_frame::<ClientMessage>(),
            Err(FrameError::Json(_))
        ));
    }

    #[test]
    fn decoder_enforces_size_limit() {
        let mut d = Decoder::new();
        let big = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(matches!(d.push(&big), Err(FrameError::TooLarge)));
    }
}
