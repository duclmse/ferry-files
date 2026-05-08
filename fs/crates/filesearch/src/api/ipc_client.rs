//! CLI-side IPC client.
//!
//! Connects to the daemon's IPC socket, sends a single request, and returns
//! the response. Uses the same length-prefixed postcard framing as the server.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::ipc_protocol::{
    decode_payload, encode_frame, DecodeError, IpcEnvelope, IpcMessage, IpcRequest, IpcResponse,
    MAX_FRAME_BYTES,
};

// ---------------------------------------------------------------------------
// Public error type
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum IpcClientError {
    Connect(std::io::Error),
    Encode(postcard::Error),
    Write(std::io::Error),
    Read(std::io::Error),
    Decode(DecodeError),
    FrameTooLarge(u32),
    DaemonError { code: String, message: String },
    UnexpectedMessage,
}

impl std::fmt::Display for IpcClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IpcClientError::Connect(e)            => write!(f, "connect: {e}"),
            IpcClientError::Encode(e)             => write!(f, "encode: {e}"),
            IpcClientError::Write(e)              => write!(f, "write: {e}"),
            IpcClientError::Read(e)               => write!(f, "read: {e}"),
            IpcClientError::Decode(e)             => write!(f, "decode: {e}"),
            IpcClientError::FrameTooLarge(n)      => write!(f, "response frame too large: {n}"),
            IpcClientError::DaemonError { code, message } =>
                write!(f, "daemon error [{code}]: {message}"),
            IpcClientError::UnexpectedMessage     => write!(f, "unexpected message from daemon"),
        }
    }
}

impl std::error::Error for IpcClientError {}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct IpcClient {
    #[cfg(unix)]
    stream: tokio::net::UnixStream,
    #[cfg(windows)]
    stream: tokio::net::windows::named_pipe::NamedPipeClient,
}

impl IpcClient {
    /// Connect to the daemon IPC socket at `ipc_path`.
    pub async fn connect(ipc_path: &Path) -> Result<Self, IpcClientError> {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(ipc_path)
                .await
                .map_err(IpcClientError::Connect)?;
            Ok(Self { stream })
        }

        #[cfg(windows)]
        {
            use tokio::net::windows::named_pipe::ClientOptions;
            let stream = ClientOptions::new()
                .open(ipc_path)
                .map_err(IpcClientError::Connect)?;
            Ok(Self { stream })
        }

        #[cfg(not(any(unix, windows)))]
        Err(IpcClientError::Connect(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "IPC not supported on this platform",
        )))
    }

    /// Send a request and receive the matching response.
    pub async fn request(&mut self, req: IpcRequest) -> Result<IpcResponse, IpcClientError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let request_id = NEXT_ID.fetch_add(1, Ordering::Relaxed) as u128;

        let env   = IpcEnvelope::new(request_id, IpcMessage::Request(req));
        let frame = encode_frame(&env).map_err(IpcClientError::Encode)?;

        self.stream
            .write_all(&frame)
            .await
            .map_err(IpcClientError::Write)?;

        let resp_env = self.read_envelope().await?;

        match resp_env.message {
            IpcMessage::Response(IpcResponse::Error { code, message }) => {
                Err(IpcClientError::DaemonError { code, message })
            }
            IpcMessage::Response(resp) => Ok(resp),
            IpcMessage::Request(_)     => Err(IpcClientError::UnexpectedMessage),
        }
    }

    async fn read_envelope(&mut self) -> Result<IpcEnvelope, IpcClientError> {
        let mut len_buf = [0u8; 4];
        self.stream
            .read_exact(&mut len_buf)
            .await
            .map_err(IpcClientError::Read)?;
        let payload_len = u32::from_le_bytes(len_buf);

        if payload_len > MAX_FRAME_BYTES {
            return Err(IpcClientError::FrameTooLarge(payload_len));
        }

        let mut payload = vec![0u8; payload_len as usize];
        self.stream
            .read_exact(&mut payload)
            .await
            .map_err(IpcClientError::Read)?;

        decode_payload(&payload).map_err(IpcClientError::Decode)
    }
}
