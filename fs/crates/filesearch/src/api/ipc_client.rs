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

impl IpcClientError {
    /// Returns `true` for transport-layer failures that warrant a reconnect.
    /// Logical daemon errors (`DaemonError`, `UnexpectedMessage`) are not
    /// connection errors and should not trigger a reconnect attempt.
    pub fn is_connection_error(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Read(_) | Self::Write(_))
    }
}

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

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn io_err(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "test")
    }

    #[test]
    fn is_connection_error_true_for_transport_variants() {
        assert!(IpcClientError::Connect(io_err(std::io::ErrorKind::ConnectionRefused)).is_connection_error());
        assert!(IpcClientError::Read(io_err(std::io::ErrorKind::BrokenPipe)).is_connection_error());
        assert!(IpcClientError::Write(io_err(std::io::ErrorKind::BrokenPipe)).is_connection_error());
    }

    #[test]
    fn is_connection_error_false_for_logical_variants() {
        assert!(!IpcClientError::UnexpectedMessage.is_connection_error());
        assert!(!IpcClientError::FrameTooLarge(1024).is_connection_error());
        assert!(!IpcClientError::DaemonError {
            code:    "NOT_FOUND".into(),
            message: "no such file".into(),
        }.is_connection_error());
    }

    #[test]
    fn display_formats_are_non_empty() {
        let variants: &[IpcClientError] = &[
            IpcClientError::Connect(io_err(std::io::ErrorKind::ConnectionRefused)),
            IpcClientError::Write(io_err(std::io::ErrorKind::BrokenPipe)),
            IpcClientError::Read(io_err(std::io::ErrorKind::UnexpectedEof)),
            IpcClientError::FrameTooLarge(999),
            IpcClientError::UnexpectedMessage,
            IpcClientError::DaemonError { code: "E".into(), message: "m".into() },
        ];
        for v in variants {
            assert!(!v.to_string().is_empty(), "Display empty for {v:?}");
        }
    }
}
