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
pub const PROTOCOL_VERSION: u16 = 9;
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024; // 16 MiB

// ---------------------------------------------------------------------------
// Top-level envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcEnvelope {
    pub magic: [u8; 4],
    pub version: u16,
    pub request_id: u128,
    pub message: IpcMessage,
}

impl IpcEnvelope {
    pub fn new(request_id: u128, message: IpcMessage) -> Self {
        Self {
            magic: MAGIC,
            version: PROTOCOL_VERSION,
            request_id,
            message,
        }
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
    Search {
        query: String,
        limit: u32,
        offset: u32,
    },
    Stats,
    Reindex {
        full: bool,
    },
    Open {
        path: String,
    },
    Extension {
        name: String,
        command: String,
    },
    /// The folders the daemon indexes, so a client can tell whether a search
    /// under some folder can be answered from the index.
    Roots,
    /// Adds an absolute folder to the watch paths, in the daemon's config
    /// file too; the daemon crawls it, then watches it. Answered with
    /// `Roots`.
    WatchAdd {
        path: String,
    },
    /// Removes a watch path, in the config file too. With `forget` its
    /// entries leave the index; otherwise they stay. Answered with `Roots`.
    WatchRemove {
        path: String,
        forget: bool,
    },
    /// Turns the connection into a stream of `Notice` responses, of the
    /// kinds asked for (all of them when empty), until the client closes it.
    /// Answered first with `Subscribed`.
    Subscribe {
        kinds: Vec<NoticeKind>,
    },
    /// The daemon's config file, or with `key` (dotted, as in
    /// `search.default_limit`) the value it runs with. Answered with
    /// `Config`.
    ConfigGet {
        key: Option<String>,
    },
    /// Changes the config file and the running daemon, which reads the file
    /// only at startup. Answered with `ConfigApplied`.
    ConfigChange {
        change: ConfigChange,
    },
}

/// A change to the daemon's config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigChange {
    /// Sets the dotted `key` to `value`, a TOML value (`200`, `"memory"`,
    /// `["zip"]`); text that is not one is taken as a string.
    Set { key: String, value: String },
    /// Removes `key` from the file, so its default applies.
    Unset { key: String },
    /// Replaces the whole file with `text`.
    Replace { text: String },
    /// Applies the file as it is now, after an edit by hand; every
    /// extension is asked to reload.
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    Search(SearchResponse),
    Stats(StatsResponse),
    Reindex {
        accepted: bool,
    },
    Open {
        opened: bool,
        temp_path: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
    Extension {
        output: String,
    },
    Roots(RootsResponse),
    Subscribed,
    Notice(Notice),
    Config(ConfigValue),
    ConfigApplied(ConfigApplied),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigValue {
    /// The whole file, or the key's value as TOML.
    pub text: String,
    /// For a key: whether the file sets it; otherwise it is the default.
    pub from_file: bool,
}

/// What a config change did. The file is written and the running daemon
/// follows it, except for what `restart` names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigApplied {
    /// Keys that differ from what the daemon started with and that it reads
    /// only at startup: they apply when it restarts.
    pub restart: Vec<String>,
    /// Extensions whose `[extensions.<name>]` changed and that took the
    /// new settings.
    pub reloaded: Vec<String>,
    /// Extensions whose section changed but that did not take it, each
    /// with why.
    pub problems: Vec<String>,
}

/// Something that changed in the daemon, sent to subscribers as it happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    /// The watch paths changed; these are the new ones.
    WatchPaths { roots: Vec<String> },
    /// A watch path's crawl moved on.
    Root {
        root: String,
        state: RootState,
        entries: u64,
    },
    /// A batch of file changes was applied: how many, and the folders they
    /// were in (at most `NOTICE_FOLDERS` of them).
    Applied { events: u32, folders: Vec<String> },
    /// This subscriber fell behind and missed this many notices: refresh
    /// what it shows.
    Lagged(u64),
}

/// Most folders an `Applied` notice names.
pub const NOTICE_FOLDERS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeKind {
    WatchPaths,
    Root,
    Applied,
}

impl Notice {
    /// Its kind; `None` for `Lagged`, which every subscriber gets.
    pub fn kind(&self) -> Option<NoticeKind> {
        match self {
            Notice::WatchPaths { .. } => Some(NoticeKind::WatchPaths),
            Notice::Root { .. } => Some(NoticeKind::Root),
            Notice::Applied { .. } => Some(NoticeKind::Applied),
            Notice::Lagged(_) => None,
        }
    }

    /// Whether a subscriber that asked for `kinds` (all when empty) gets it.
    pub fn wanted(&self, kinds: &[NoticeKind]) -> bool {
        kinds.is_empty() || self.kind().is_none_or(|k| kinds.contains(&k))
    }
}

impl NoticeKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "watch_paths" => Some(NoticeKind::WatchPaths),
            "root" => Some(NoticeKind::Root),
            "applied" => Some(NoticeKind::Applied),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Response DTOs - smaller than full FileRecord
// ---------------------------------------------------------------------------

/// Compact search hit sent over IPC; does not carry internal DB ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHitDto {
    pub path: String,
    pub name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub is_dir: bool,
    /// [`crate::attrib`] bits.
    pub attrib_bitmask: u32,
    pub archive_path: Option<String>,
    pub snippet: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHitDto>,
    pub total_hits: u64,
    pub limit: u32,
    pub offset: u32,
    pub capped: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootsResponse {
    /// Watch paths as the index stores them (native separators).
    pub roots: Vec<String>,
    /// False until a full crawl has finished once; until then the index
    /// misses files and a client should not trust an empty result.
    pub crawled: bool,
    /// Each of `roots`, in the same order: whether its crawl is done.
    pub status: Vec<RootStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RootStatus {
    pub state: RootState,
    /// Entries its last or running crawl wrote.
    pub entries: u64,
}

/// Where a watch path's crawl is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RootState {
    /// Added, its crawl not started yet.
    Pending,
    Crawling,
    /// Crawled, and followed live.
    Ready,
    /// Not reachable: its entries stay as they were.
    Offline,
}

impl RootState {
    pub fn label(self) -> &'static str {
        match self {
            RootState::Pending => "pending",
            RootState::Crawling => "crawling",
            RootState::Ready => "ready",
            RootState::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    pub total_files: u64,
    pub total_dirs: u64,
    pub content_indexed: u64,
    pub archive_entries_indexed: u64,
    pub archives_crawled: u64,
    pub events_processed: u64,
    /// Unix time the last full crawl finished; 0 before the first.
    pub last_crawl_secs: u64,
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
    // Magic and version are checked before the message: a peer on another
    // version may send a message this one cannot decode, and should be told
    // VersionMismatch, not a postcard error.
    let ((magic, version), _) =
        postcard::take_from_bytes::<([u8; 4], u16)>(bytes).map_err(DecodeError::Postcard)?;
    if magic != MAGIC {
        return Err(DecodeError::BadMagic(magic));
    }
    if version != PROTOCOL_VERSION {
        return Err(DecodeError::VersionMismatch(version));
    }
    postcard::from_bytes(bytes).map_err(DecodeError::Postcard)
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
            DecodeError::Postcard(e) => write!(f, "postcard decode: {e}"),
            DecodeError::FrameTooLarge(n) => write!(f, "frame too large: {n} bytes"),
            DecodeError::BadMagic(m) => write!(f, "bad magic: {m:?}"),
            DecodeError::VersionMismatch(v) => write!(f, "unsupported version: {v}"),
            DecodeError::Io(e) => write!(f, "io: {e}"),
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
            query: "foo".into(),
            limit: 10,
            offset: 0,
        }));
        assert_eq!(env.magic, MAGIC);
        assert_eq!(env.version, PROTOCOL_VERSION);
        assert_eq!(env.request_id, 42);
        assert!(matches!(
            env.message,
            IpcMessage::Request(IpcRequest::Search { .. })
        ));
    }

    #[test]
    fn round_trip_stats() {
        let env = round_trip(IpcMessage::Request(IpcRequest::Stats));
        assert!(matches!(
            env.message,
            IpcMessage::Request(IpcRequest::Stats)
        ));
    }

    /// Clients built against an older fs-protocol (Ferry Files bundles one
    /// fsd but talks to whichever is running) rely on these bytes; a change
    /// here must bump PROTOCOL_VERSION.
    #[test]
    fn bytes_other_clients_rely_on() {
        let frame = |m| encode_frame(&IpcEnvelope::new(1, m)).unwrap();
        assert_eq!(
            frame(IpcMessage::Request(IpcRequest::Roots)),
            b"\x08\0\0\0ZFS1\x09\x01\x00\x05"
        );
        assert_eq!(
            frame(IpcMessage::Request(IpcRequest::Search {
                query: "a".into(),
                limit: 2,
                offset: 3,
            })),
            b"\x0c\0\0\0ZFS1\x09\x01\x00\x00\x01a\x02\x03"
        );
        assert_eq!(
            frame(IpcMessage::Response(IpcResponse::Roots(RootsResponse {
                roots: vec!["C".into()],
                crawled: true,
                status: vec![],
            }))),
            b"\x0d\0\0\0ZFS1\x09\x01\x01\x06\x01\x01C\x01\x00"
        );
    }

    #[test]
    fn notices_round_trip_and_filter_by_kind() {
        let n = Notice::Root {
            root: "/a".into(),
            state: RootState::Ready,
            entries: 3,
        };
        match round_trip(IpcMessage::Response(IpcResponse::Notice(n.clone()))).message {
            IpcMessage::Response(IpcResponse::Notice(back)) => assert_eq!(back, n),
            other => panic!("unexpected {other:?}"),
        }
        assert!(n.wanted(&[]));
        assert!(n.wanted(&[NoticeKind::Root]));
        assert!(!n.wanted(&[NoticeKind::Applied]));
        assert!(Notice::Lagged(2).wanted(&[NoticeKind::Applied]));
        assert_eq!(NoticeKind::parse("applied"), Some(NoticeKind::Applied));
    }

    #[test]
    fn round_trip_roots() {
        let env = round_trip(IpcMessage::Request(IpcRequest::Roots));
        assert!(matches!(
            env.message,
            IpcMessage::Request(IpcRequest::Roots)
        ));
        let env = round_trip(IpcMessage::Response(IpcResponse::Roots(RootsResponse {
            roots: vec![r"C:\Users".into()],
            crawled: true,
            status: vec![RootStatus {
                state: RootState::Crawling,
                entries: 7,
            }],
        })));
        match env.message {
            IpcMessage::Response(IpcResponse::Roots(r)) => {
                assert_eq!(r.roots, [r"C:\Users"]);
                assert!(r.crawled);
                assert_eq!(r.status[0].state, RootState::Crawling);
                assert_eq!(r.status[0].entries, 7);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn round_trip_config() {
        let change = ConfigChange::Set {
            key: "search.default_limit".into(),
            value: "200".into(),
        };
        let env = round_trip(IpcMessage::Request(IpcRequest::ConfigChange {
            change: change.clone(),
        }));
        match env.message {
            IpcMessage::Request(IpcRequest::ConfigChange { change: c }) => assert_eq!(c, change),
            other => panic!("unexpected {other:?}"),
        }
        let applied = ConfigApplied {
            restart: vec!["data_dir".into()],
            reloaded: vec!["rar2zip".into()],
            problems: Vec::new(),
        };
        let env = round_trip(IpcMessage::Response(IpcResponse::ConfigApplied(
            applied.clone(),
        )));
        match env.message {
            IpcMessage::Response(IpcResponse::ConfigApplied(a)) => assert_eq!(a, applied),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn round_trip_search_response() {
        let resp = IpcResponse::Search(SearchResponse {
            hits: vec![SearchHitDto {
                path: "/a/b.rs".into(),
                name: "b.rs".into(),
                extension: Some("rs".into()),
                size_bytes: 1024,
                mtime: 0,
                ctime: 0,
                is_dir: false,
                attrib_bitmask: 0,
                archive_path: None,
                snippet: Some("fn main".into()),
            }],
            total_hits: 1,
            limit: 10,
            offset: 0,
            capped: false,
        });
        let env = round_trip(IpcMessage::Response(resp));
        assert!(matches!(
            env.message,
            IpcMessage::Response(IpcResponse::Search(_))
        ));
    }

    #[test]
    fn round_trip_error_response() {
        let env = round_trip(IpcMessage::Response(IpcResponse::Error {
            code: "NOT_FOUND".into(),
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
            magic: *b"XXXX",
            version: PROTOCOL_VERSION,
            request_id: 1,
            message: IpcMessage::Request(IpcRequest::Stats),
        };
        let payload = postcard::to_allocvec(&env).unwrap();
        let err = decode_payload(&payload).unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic(_)));
    }

    #[test]
    fn version_mismatch_rejected() {
        let env = IpcEnvelope {
            magic: MAGIC,
            version: 99,
            request_id: 1,
            message: IpcMessage::Request(IpcRequest::Stats),
        };
        let payload = postcard::to_allocvec(&env).unwrap();
        let err = decode_payload(&payload).unwrap_err();
        assert!(matches!(err, DecodeError::VersionMismatch(99)));
    }

    #[test]
    fn version_checked_before_message() {
        // Version 10 with a message variant this version doesn't have.
        let err = decode_payload(b"ZFS1\x0a\x01\x01\x7f").unwrap_err();
        assert!(matches!(err, DecodeError::VersionMismatch(10)), "{err}");
        let err = decode_payload(b"XXXX\x09").unwrap_err();
        assert!(matches!(err, DecodeError::BadMagic(_)), "{err}");
    }

    #[test]
    fn binary_response_smaller_than_json_for_100_hits() {
        let hits: Vec<SearchHitDto> = (0..100)
            .map(|i| SearchHitDto {
                path: format!("/some/long/path/to/file_{i}.rs"),
                name: format!("file_{i}.rs"),
                extension: Some("rs".into()),
                size_bytes: i * 1024,
                mtime: 1_700_000_000 + i as i64,
                ctime: 1_600_000_000,
                is_dir: false,
                attrib_bitmask: crate::attrib::ARCHIVE,
                archive_path: None,
                snippet: None,
            })
            .collect();

        let resp = SearchResponse {
            hits,
            total_hits: 100,
            limit: 100,
            offset: 0,
            capped: false,
        };
        let env = IpcEnvelope::new(1, IpcMessage::Response(IpcResponse::Search(resp.clone())));

        let binary_frame = encode_frame(&env).unwrap();
        let json_bytes = serde_json::to_vec(&resp).unwrap();

        assert!(
            binary_frame.len() < json_bytes.len(),
            "postcard frame ({} bytes) should be smaller than JSON ({} bytes)",
            binary_frame.len(),
            json_bytes.len()
        );
    }
}
