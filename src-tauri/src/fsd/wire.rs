//! fsd's IPC protocol as the fs project's `fs-protocol` crate defines it: a u32 little-endian
//! length, then a postcard-encoded envelope. This is a copy, so Ferry builds without the fs
//! sources. The enums keep fs's variant order, since postcard sends a variant by its position;
//! `bytes_fsd_expects` checks the same bytes as fs-protocol's `bytes_other_clients_rely_on`.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAGIC: [u8; 4] = *b"ZFS1";
/// fs-protocol's `PROTOCOL_VERSION`. fsd refuses any other.
const VERSION: u16 = 9;
const MAX_FRAME: u32 = 16 * 1024 * 1024;
/// fs-protocol's `attrib::HIDDEN`.
pub const HIDDEN: u32 = 16;

#[derive(Serialize, Deserialize)]
struct Envelope {
    magic: [u8; 4],
    version: u16,
    request_id: u128,
    message: Message,
}

#[derive(Serialize, Deserialize)]
enum Message {
    Request(Request),
    Response(Response),
}

// Variants Ferry never sends still hold their places.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub enum Request {
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
    Roots,
    WatchAdd {
        path: String,
    },
    WatchRemove {
        path: String,
        forget: bool,
    },
    /// Never sent; fs's is `Subscribe { kinds: Vec<NoticeKind> }`.
    Subscribe,
    ConfigGet {
        key: Option<String>,
    },
    /// Changes fsd's settings file and the running fsd. Answered with `ConfigApplied`.
    ConfigChange {
        change: ConfigChange,
    },
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub enum ConfigChange {
    Set {
        key: String,
        value: String,
    },
    Unset {
        key: String,
    },
    Replace {
        text: String,
    },
    /// Applies the file as it is now, after it was edited by hand.
    Reload,
}

/// What a config change did: fsd follows the file now, except for what `restart` names.
#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct ConfigApplied {
    /// Settings fsd reads only when it starts, which differ from what it started with.
    pub restart: Vec<String>,
    pub reloaded: Vec<String>,
    pub problems: Vec<String>,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub enum Response {
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
    /// Never asked for (Ferry doesn't subscribe); fs's carries a `Notice`.
    Notice,
    Config {
        text: String,
        from_file: bool,
    },
    ConfigApplied(ConfigApplied),
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct SearchHit {
    pub path: String,
    pub name: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub is_dir: bool,
    pub attrib_bitmask: u32,
    /// The ZIP it's inside, for a file in one.
    pub archive_path: Option<String>,
    pub snippet: Option<String>,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub total_hits: u64,
    pub limit: u32,
    pub offset: u32,
    pub capped: bool,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct StatsResponse {
    total_files: u64,
    total_dirs: u64,
    content_indexed: u64,
    archive_entries_indexed: u64,
    archives_crawled: u64,
    events_processed: u64,
    last_crawl_secs: u64,
}

#[derive(Serialize, Deserialize)]
pub struct RootsResponse {
    /// fsd's watch paths, the computer's paths.
    pub roots: Vec<String>,
    /// False until fsd has crawled them once.
    pub crawled: bool,
    /// Each of `roots`: where its crawl is.
    pub status: Vec<RootStatus>,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize)]
pub struct RootStatus {
    pub state: RootState,
    pub entries: u64,
}

#[allow(dead_code)]
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub enum RootState {
    Pending,
    Crawling,
    Ready,
    Offline,
}

/// Where fsd listens unless its config says otherwise (fs-protocol's `default_ipc_path`).
fn default_path() -> std::path::PathBuf {
    if cfg!(windows) {
        r"\\.\pipe\fs".into()
    } else {
        "/tmp/fs.sock".into()
    }
}

fn frame(request_id: u128, message: Message) -> Result<Vec<u8>, String> {
    let payload = postcard::to_allocvec(&Envelope {
        magic: MAGIC,
        version: VERSION,
        request_id,
        message,
    })
    .map_err(|e| e.to_string())?;
    let mut out = (payload.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&payload);
    Ok(out)
}

fn unframe(payload: &[u8]) -> Result<Envelope, String> {
    // The magic and version come first, so they can be checked before the rest is understood.
    match payload {
        [m0, m1, m2, m3, ..] if [*m0, *m1, *m2, *m3] != MAGIC => return Err("this isn't fsd".into()),
        [_, _, _, _, v, ..] if *v != VERSION as u8 => {
            return Err(format!("fsd speaks protocol version {v} and Ferry Files version {VERSION}; update the older one"))
        }
        _ => {}
    }
    postcard::from_bytes(payload).map_err(|e| format!("couldn't read fsd's answer: {e}"))
}

pub struct Client {
    #[cfg(unix)]
    stream: tokio::net::UnixStream,
    #[cfg(windows)]
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
    next_id: u128,
}

impl Client {
    pub async fn connect() -> std::io::Result<Client> {
        let path = default_path();
        #[cfg(unix)]
        let stream = tokio::net::UnixStream::connect(&path).await?;
        #[cfg(windows)]
        let stream = tokio::net::windows::named_pipe::ClientOptions::new().open(&path)?;
        Ok(Client { stream, next_id: 1 })
    }

    pub async fn request(&mut self, request: Request) -> Result<Response, String> {
        let id = self.next_id;
        self.next_id += 1;
        let io = |e: std::io::Error| format!("talking to fsd: {e}");
        self.stream
            .write_all(&frame(id, Message::Request(request))?)
            .await
            .map_err(io)?;
        let mut len = [0u8; 4];
        self.stream.read_exact(&mut len).await.map_err(io)?;
        let len = u32::from_le_bytes(len);
        if len > MAX_FRAME {
            return Err(format!("fsd's answer is too big ({len} bytes)"));
        }
        let mut payload = vec![0u8; len as usize];
        self.stream.read_exact(&mut payload).await.map_err(io)?;
        match unframe(&payload)?.message {
            Message::Response(Response::Error { code, message }) => {
                Err(format!("fsd: {message} ({code})"))
            }
            Message::Response(r) => Ok(r),
            Message::Request(_) => Err("fsd sent a request".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_fsd_expects() {
        let frame = |m| frame(1, m).unwrap();
        assert_eq!(
            frame(Message::Request(Request::Roots)),
            b"\x08\0\0\0ZFS1\x09\x01\x00\x05"
        );
        assert_eq!(
            frame(Message::Request(Request::Search {
                query: "a".into(),
                limit: 2,
                offset: 3
            })),
            b"\x0c\0\0\0ZFS1\x09\x01\x00\x00\x01a\x02\x03"
        );
        assert_eq!(
            frame(Message::Request(Request::ConfigChange {
                change: ConfigChange::Reload
            })),
            b"\x09\0\0\0ZFS1\x09\x01\x00\x0a\x03"
        );
        let applied = b"ZFS1\x09\x01\x01\x0a\x01\x05store\x00\x00";
        match unframe(applied).unwrap().message {
            Message::Response(Response::ConfigApplied(a)) => assert_eq!(a.restart, ["store"]),
            _ => panic!("not a ConfigApplied answer"),
        }
        let answer = b"\x0d\0\0\0ZFS1\x09\x01\x01\x06\x01\x01C\x01\x00";
        match unframe(&answer[4..]).unwrap().message {
            Message::Response(Response::Roots(r)) => {
                assert_eq!(r.roots, ["C"]);
                assert!(r.crawled);
            }
            _ => panic!("not a Roots answer"),
        }
    }

    #[test]
    fn another_version_is_named() {
        let e = unframe(b"ZFS1\x0a\x01\x01\x06\x00\x00").err().unwrap();
        assert!(e.contains("version 10"), "{e}");
        assert!(unframe(b"XXXX\x08").is_err());
    }
}
