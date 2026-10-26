use crate::protocol::*;
use crate::server::default_socket_path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

pub const IPC_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("IO error connecting to IPC socket: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Server error: {0}")]
    Server(String),
    #[error("Server closed connection unexpectedly")]
    ConnectionClosed,
    #[error("IPC message exceeds the {0} byte line limit")]
    LineTooLong(usize),
    #[error(
        "IPC protocol mismatch: server supports up to version {server}, this client uses version {client}; rebuild both from the same tree"
    )]
    ProtocolMismatch { server: u32, client: u32 },
    #[error("IPC operation timed out: {0}")]
    Timeout(String),
}

impl From<tokio::time::error::Elapsed> for ClientError {
    fn from(_: tokio::time::error::Elapsed) -> Self {
        ClientError::Timeout("operation exceeded 5s".into())
    }
}

async fn read_line_capped<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
) -> Result<(), ClientError> {
    debug_assert!(line.len() <= IPC_MAX_LINE_BYTES);
    let remaining = IPC_MAX_LINE_BYTES + 1 - line.len();
    reader
        .take(remaining as u64)
        .read_until(b'\n', line)
        .await?;
    if line.len() > IPC_MAX_LINE_BYTES {
        line.clear();
        return Err(ClientError::LineTooLong(IPC_MAX_LINE_BYTES));
    }
    if line.last() == Some(&b'\n') {
        return Ok(());
    }

    line.clear();
    Err(ClientError::ConnectionClosed)
}

fn decode_response(line: &[u8]) -> Result<IpcResponse, ClientError> {
    match serde_json::from_slice::<IpcResponse>(line)? {
        IpcResponse::Error(msg) => Err(ClientError::Server(msg)),
        IpcResponse::UnsupportedProtocol {
            supported,
            received,
        } => Err(ClientError::ProtocolMismatch {
            server: supported,
            client: received,
        }),
        response => Ok(response),
    }
}

pub struct IpcClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl IpcClient {
    pub async fn connect() -> Result<Self, ClientError> {
        let path = default_socket_path();
        Self::connect_to(path).await
    }

    pub async fn connect_to(path: PathBuf) -> Result<Self, ClientError> {
        let stream = tokio::time::timeout(IPC_TIMEOUT, UnixStream::connect(path)).await??;
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        Ok(Self {
            reader,
            writer: write_half,
        })
    }

    pub async fn send(&mut self, request: IpcRequest) -> Result<IpcResponse, ClientError> {
        let mut req_str = serde_json::to_string(&IpcRequestEnvelope::new(request))?;
        req_str.push('\n');
        tokio::time::timeout(IPC_TIMEOUT, self.writer.write_all(req_str.as_bytes())).await??;

        let mut line = Vec::new();
        tokio::time::timeout(IPC_TIMEOUT, read_line_capped(&mut self.reader, &mut line)).await??;
        decode_response(&line)
    }

    pub async fn subscribe() -> Result<EventSubscription, ClientError> {
        Self::subscribe_to(default_socket_path()).await
    }

    pub async fn subscribe_to(path: PathBuf) -> Result<EventSubscription, ClientError> {
        let mut client = Self::connect_to(path).await?;
        let mut req_str =
            serde_json::to_string(&IpcRequestEnvelope::new(IpcRequest::SubscribeEvents))?;
        req_str.push('\n');
        tokio::time::timeout(IPC_TIMEOUT, client.writer.write_all(req_str.as_bytes())).await??;

        let mut ack = Vec::new();
        tokio::time::timeout(IPC_TIMEOUT, read_line_capped(&mut client.reader, &mut ack)).await??;
        match decode_response(&ack)? {
            IpcResponse::Ok => Ok(EventSubscription {
                reader: client.reader,
                pending: Vec::new(),
            }),
            other => Err(ClientError::Server(format!(
                "unexpected subscribe handshake response: {:?}",
                other
            ))),
        }
    }
}

pub struct EventSubscription {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,

    pending: Vec<u8>,
}

impl EventSubscription {

    pub async fn next_event(&mut self) -> Result<IpcEvent, ClientError> {
        read_line_capped(&mut self.reader, &mut self.pending).await?;
        let parsed = serde_json::from_slice(&self.pending);
        self.pending.clear();
        Ok(parsed?)
    }

    pub async fn next_event_timeout(&mut self, timeout: Duration) -> Result<IpcEvent, ClientError> {
        match tokio::time::timeout(timeout, self.next_event()).await {
            Ok(result) => result,
            Err(_) => Err(ClientError::Timeout(format!(
                "no IPC event within {timeout:?}"
            ))),
        }
    }
}
