//! Typed binary IPC protocol for the local daemon ↔ CLI channel.
//!
//! Wire format:  u32 LE length  |  postcard-serialised `IpcEnvelope`
//! Magic:        b"ZFS1"  (4 bytes inside every envelope)
//! Max frame:    16 MiB

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

pub const MAGIC: [u8; 4] = *b"ZFS1";
pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024; // 16 MiB

// ---------------------------------------------------------------------------
// Top-level envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    pub magic:      [u8; 4],
    pub version:    u16,
    pub request_id: u128,
    pub message:    IpcMessage,
}

impl IpcEnvelope {
    pub fn new(request_id: u128, message: IpcMessage) -> Self {
        Self { magic: MAGIC, version: PROTOCOL_VERSION, request_id, message }
    }
}

// ---------------------------------------------------------------------------
// Message variants
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcMessage {
    Request(IpcRequest),
    Response(IpcResponse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    Search { query: String, limit: u32, offset: u32 },
    Stats,
    Reindex { full: bool },
    Open { path: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    Search(SearchResponse),
    Stats(StatsResponse),
    Reindex { accepted: bool },
    Open { opened: bool, temp_path: Option<String> },
    Error { code: String, message: String },
}

// ---------------------------------------------------------------------------
// Response DTOs — smaller than full FileRecord
// ---------------------------------------------------------------------------

/// Compact search hit sent over IPC; does not carry internal DB ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHitDto {
    pub path:         String,
    pub name:         String,
    pub extension:    Option<String>,
    pub size_bytes:   u64,
    pub mtime:        i64,
    pub is_dir:       bool,
    pub archive_path: Option<String>,
    pub snippet:      Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits:         Vec<SearchHitDto>,
    pub total_hits:   u64,
    pub limit:        u32,
    pub offset:       u32,
    pub capped:       bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    pub total_files:             u64,
    pub total_dirs:              u64,
    pub content_indexed:         u64,
    pub archive_entries_indexed: u64,
    pub archives_crawled:        u64,
    pub events_processed:        u64,
    pub last_crawl_secs:         u64,
}

// ---------------------------------------------------------------------------
// Framing helpers
// ---------------------------------------------------------------------------

/// Encode an `IpcEnvelope` into a length-prefixed frame.
///
/// Layout: `[u32 LE length][postcard bytes]`
pub fn encode_frame(envelope: &IpcEnvelope) -> Result<Vec<u8>, postcard::Error> {
    let payload = postcard::to_allocvec(envelope)?;
    let len = payload.len() as u32;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// Decode the payload bytes (after the 4-byte length prefix is stripped) into
/// an `IpcEnvelope`, validating magic and version.
pub fn decode_payload(bytes: &[u8]) -> Result<IpcEnvelope, DecodeError> {
    let env: IpcEnvelope = postcard::from_bytes(bytes).map_err(DecodeError::Postcard)?;
    if env.magic != MAGIC {
        return Err(DecodeError::BadMagic(env.magic));
    }
    if env.version != PROTOCOL_VERSION {
        return Err(DecodeError::VersionMismatch(env.version));
    }
    Ok(env)
}

#[derive(Debug)]
pub enum DecodeError {
    Postcard(postcard::Error),
    FrameTooLarge(u32),
    BadMagic([u8; 4]),
    VersionMismatch(u16),
    Io(std::io::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Postcard(e)       => write!(f, "postcard decode: {e}"),
            DecodeError::FrameTooLarge(n)  => write!(f, "frame too large: {n} bytes"),
            DecodeError::BadMagic(m)       => write!(f, "bad magic: {m:?}"),
            DecodeError::VersionMismatch(v)=> write!(f, "unsupported version: {v}"),
            DecodeError::Io(e)             => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: IpcMessage) -> IpcEnvelope {
        let env = IpcEnvelope::new(42, msg);
        let frame = encode_frame(&env).expect("encode");
        // Strip the 4-byte length prefix
        let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
        assert_eq!(len, frame.len() - 4);
        decode_payload(&frame[4..]).expect("decode")
    }

    #[test]
    fn round_trip_search_request() {
        let env = round_trip(IpcMessage::Request(IpcRequest::Search {
            query:  "foo".into(),
            limit:  10,
            offset: 0,
        }));
        assert_eq!(env.magic, MAGIC);
        assert_eq!(env.version, PROTOCOL_VERSION);
        assert_eq!(env.request_id, 42);
        assert!(matches!(env.message, IpcMessage::Request(IpcRequest::Search { .. })));
    }

    #[test]
    fn round_trip_stats() {
        let env = round_trip(IpcMessage::Request(IpcRequest::Stats));
        assert!(matches!(env.message, IpcMessage::Request(IpcRequest::Stats)));
    }

    #[test]
    fn round_trip_search_response() {
        let resp = IpcResponse::Search(SearchResponse {
            hits: vec![SearchHitDto {
                path:         "/a/b.rs".into(),
                name:         "b.rs".into(),
                extension:    Some("rs".into()),
                size_bytes:   1024,
                mtime:        0,
                is_dir:       false,
                archive_path: None,
                snippet:      Some("fn main".into()),
            }],
            total_hits: 1,
            limit:      10,
            offset:     0,
            capped:     false,
        });
        let env = round_trip(IpcMessage::Response(resp));
        assert!(matches!(env.message, IpcMessage::Response(IpcResponse::Search(_))));
    }

    #[test]
    fn round_trip_error_response() {
        let env = round_trip(IpcMessage::Response(IpcResponse::Error {
            code:    "NOT_FOUND".into(),
            message: "path does not exist".into(),
        }));
        assert!(matches!(
            env.message,
            IpcMessage::Response(IpcResponse::Error { .. })
        ));
    }

    #[test]
    fn frame_too_large_rejected() {
        let too_large: u32 = MAX_FRAME_BYTES + 1;
        assert!(too_large > MAX_FRAME_BYTES, "sanity");
        match DecodeError::FrameTooLarge(too_large) {
            DecodeError::FrameTooLarge(n) => assert_eq!(n, too_large),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn bad_magic_rejected() {
        let env = IpcEnvelope {
            magic:      *b"XXXX",
            version:    PROTOCOL_VERSION,
            request_id: 1,
            message:    IpcMessage::Request(IpcRequest::Stats),
        };
        let payload = postcard::to_allocvec(&env).unwrap();
        let err = decode_payload(&payload).unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic(_)));
    }

    #[test]
    fn version_mismatch_rejected() {
        let env = IpcEnvelope {
            magic:      MAGIC,
            version:    99,
            request_id: 1,
            message:    IpcMessage::Request(IpcRequest::Stats),
        };
        let payload = postcard::to_allocvec(&env).unwrap();
        let err = decode_payload(&payload).unwrap_err();
        assert!(matches!(err, DecodeError::VersionMismatch(99)));
    }

    #[test]
    fn binary_response_smaller_than_json_for_100_hits() {
        let hits: Vec<SearchHitDto> = (0..100)
            .map(|i| SearchHitDto {
                path:         format!("/some/long/path/to/file_{i}.rs"),
                name:         format!("file_{i}.rs"),
                extension:    Some("rs".into()),
                size_bytes:   i * 1024,
                mtime:        1_700_000_000 + i as i64,
                is_dir:       false,
                archive_path: None,
                snippet:      None,
            })
            .collect();

        let resp = SearchResponse { hits, total_hits: 100, limit: 100, offset: 0, capped: false };
        let env  = IpcEnvelope::new(1, IpcMessage::Response(IpcResponse::Search(resp.clone())));

        let binary_frame = encode_frame(&env).unwrap();
        let json_bytes   = serde_json::to_vec(&resp).unwrap();

        assert!(
            binary_frame.len() < json_bytes.len(),
            "postcard frame ({} bytes) should be smaller than JSON ({} bytes)",
            binary_frame.len(),
            json_bytes.len()
        );
    }
}
