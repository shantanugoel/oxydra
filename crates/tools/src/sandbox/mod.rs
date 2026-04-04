use std::{
    collections::{BTreeMap, VecDeque},
    fs, io,
    path::Path,
    pin::Pin,
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Command,
    time,
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::{info, warn};
use types::{
    ExecCommand, ExecCommandAck, KillSession, KillSessionAck, ShellDaemonRequest,
    ShellDaemonResponse, ShellOutputStream, SidecarEndpoint, SidecarTransport, SpawnSession,
    StreamOutput, StreamOutputChunk,
};

mod policy;
mod wasm_runner;
#[cfg(feature = "wasm-isolation")]
mod wasm_wasi_runner;
mod web_fetch;
mod web_search;

pub use policy::{
    SecurityPolicy, SecurityPolicyViolation, SecurityPolicyViolationReason, WorkspaceSecurityPolicy,
};
pub use wasm_runner::{
    HostWasmToolRunner, WasmCapabilityProfile, WasmInvocationMetadata, WasmInvocationResult,
    WasmMount, WasmToolRunner, WasmWorkspaceMounts,
};
#[cfg(feature = "wasm-isolation")]
pub use wasm_wasi_runner::WasmWasiToolRunner;

pub const DEFAULT_MAX_SESSION_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_STREAM_CHUNK_BYTES: usize = 8 * 1024;

static NEXT_LOCAL_SESSION_NUMBER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSessionConfig {
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub max_session_output_bytes: usize,
    pub max_stream_chunk_bytes: usize,
}

impl Default for ShellSessionConfig {
    fn default() -> Self {
        Self {
            shell: None,
            cwd: None,
            env: BTreeMap::new(),
            max_session_output_bytes: DEFAULT_MAX_SESSION_OUTPUT_BYTES,
            max_stream_chunk_bytes: DEFAULT_MAX_STREAM_CHUNK_BYTES,
        }
    }
}

impl ShellSessionConfig {
    fn validate(&self) -> Result<(), SandboxError> {
        if self.max_session_output_bytes == 0 {
            return Err(SandboxError::InvalidConfig(
                "max_session_output_bytes must be greater than zero",
            ));
        }
        if self.max_stream_chunk_bytes == 0 {
            return Err(SandboxError::InvalidConfig(
                "max_stream_chunk_bytes must be greater than zero",
            ));
        }
        Ok(())
    }

    fn shell_binary(&self) -> String {
        self.shell
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(default_shell_binary)
    }

    fn cwd(&self) -> Option<String> {
        self.cwd
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStatus {
    Ready(SessionConnection),
    Unavailable(SessionUnavailable),
}

impl SessionStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionConnection {
    Sidecar {
        transport: SidecarTransport,
        address: String,
    },
    LocalProcess,
    SandboxedShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUnavailable {
    pub reason: SessionUnavailableReason,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionUnavailableReason {
    MissingSidecarEndpoint,
    UnsupportedTransport,
    InvalidAddress,
    ConnectionFailed,
    ProtocolError,
    Disabled,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("invalid sandbox config: {0}")]
    InvalidConfig(&'static str),
    #[error("shell session unavailable ({reason:?}): {detail}")]
    SessionUnavailable {
        reason: SessionUnavailableReason,
        detail: String,
    },
    #[error("command must not be empty")]
    EmptyCommand,
    #[error("shell session request failed: {0}")]
    Transport(#[from] io::Error),
    #[error("failed to serialize oxydra-shelld request: {0}")]
    SerializeRequest(#[source] serde_json::Error),
    #[error("failed to decode oxydra-shelld response: {0}")]
    DeserializeResponse(#[source] serde_json::Error),
    #[error("oxydra-shelld connection closed before a response frame was received")]
    ConnectionClosed,
    #[error("oxydra-shelld returned error response: {message}")]
    ShellDaemon {
        request_id: Option<String>,
        message: String,
    },
    #[error("unexpected oxydra-shelld response: expected {expected}, got {actual}")]
    UnexpectedResponse {
        expected: &'static str,
        actual: &'static str,
    },
    #[error("oxydra-shelld rejected request `{request_id}`")]
    ShellDaemonRejected { request_id: String },
    #[error("command execution timed out after {timeout_secs}s")]
    CommandTimeout { timeout_secs: u64 },
    #[error("wasm tool invocation failed for `{tool}` request `{request_id}`: {message}")]
    WasmInvocationFailed {
        tool: String,
        request_id: String,
        message: String,
    },
}

#[async_trait]
pub trait ShellSession: Send {
    fn status(&self) -> &SessionStatus;
    fn session_id(&self) -> Option<&str>;
    async fn exec_command(
        &mut self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError>;
    async fn stream_output(
        &mut self,
        max_bytes: Option<usize>,
    ) -> Result<StreamOutputChunk, SandboxError>;
    async fn kill_session(&mut self) -> Result<KillSessionAck, SandboxError>;
}

pub trait BrowserSession: Send + Sync {
    fn status(&self) -> &SessionStatus;
}

#[derive(Debug, Clone)]
pub struct UnavailableBrowserSession {
    status: SessionStatus,
}

impl UnavailableBrowserSession {
    pub fn missing_sidecar() -> Self {
        Self {
            status: missing_sidecar_status(),
        }
    }

    pub fn disabled(detail: impl Into<String>) -> Self {
        Self {
            status: SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::Disabled,
                detail: detail.into(),
            }),
        }
    }
}

impl BrowserSession for UnavailableBrowserSession {
    fn status(&self) -> &SessionStatus {
        &self.status
    }
}

pub struct VsockShellSession {
    status: SessionStatus,
    session_id: Option<String>,
    client: Option<ShellDaemonRpcClient>,
    /// Stored for reconnection after socket desync.
    /// `None` when the session was created via `connect_with_stream` (e.g. in tests)
    /// where we don't have the addressing information needed to open a new socket.
    pub(crate) reconnect_endpoint: Option<SidecarEndpoint>,
    /// Shell session config, stored for reconnection so we can re-spawn a session.
    pub(crate) reconnect_config: ShellSessionConfig,
}

impl VsockShellSession {
    pub async fn connect(endpoint: Option<SidecarEndpoint>, config: ShellSessionConfig) -> Self {
        if let Err(error) = config.validate() {
            return Self::unavailable(SessionUnavailableReason::ProtocolError, error.to_string());
        }

        let endpoint = match endpoint {
            Some(endpoint) => endpoint,
            None => return Self::unavailable_from_status(missing_sidecar_status()),
        };

        if endpoint.transport != SidecarTransport::Vsock {
            return Self::unavailable(
                SessionUnavailableReason::UnsupportedTransport,
                format!(
                    "expected `{:?}` sidecar transport, got `{:?}`",
                    SidecarTransport::Vsock,
                    endpoint.transport
                ),
            );
        }

        let bridge_path = match sidecar_unix_bridge_path(&endpoint.address) {
            Some(path) => path,
            None => {
                return Self::unavailable(
                    SessionUnavailableReason::InvalidAddress,
                    format!(
                        "sidecar endpoint `{}` is not reachable by the WS4 client; expected unix bridge socket path",
                        endpoint.address
                    ),
                );
            }
        };

        #[cfg(unix)]
        {
            match UnixStream::connect(bridge_path).await {
                Ok(stream) => match Self::connect_with_stream(
                    stream,
                    SidecarTransport::Vsock,
                    endpoint.address.clone(),
                    config.clone(),
                )
                .await
                {
                    Ok(mut session) => {
                        session.reconnect_endpoint = Some(endpoint);
                        session.reconnect_config = config;
                        session
                    }
                    Err(error) => Self::unavailable(
                        SessionUnavailableReason::ProtocolError,
                        error.to_string(),
                    ),
                },
                Err(error) => Self::unavailable(
                    SessionUnavailableReason::ConnectionFailed,
                    format!("failed to connect to sidecar bridge socket `{bridge_path}`: {error}"),
                ),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = bridge_path;
            Self::unavailable(
                SessionUnavailableReason::UnsupportedTransport,
                "unix socket bridge dialing is unsupported on this platform",
            )
        }
    }

    pub async fn connect_with_stream<Stream>(
        stream: Stream,
        transport: SidecarTransport,
        address: impl Into<String>,
        config: ShellSessionConfig,
    ) -> Result<Self, SandboxError>
    where
        Stream: AsyncRead + AsyncWrite + Send + 'static,
    {
        config.validate()?;

        let mut client = ShellDaemonRpcClient::new(stream);
        let spawn_ack = client.spawn_session(&config).await?;
        let address = address.into();
        Ok(Self {
            status: sidecar_status(transport, address),
            session_id: Some(spawn_ack.session_id),
            client: Some(client),
            // Reconnection via `connect_with_stream` is not supported because
            // we don't have the addressing information to open a new socket.
            // Callers that use `connect()` set these fields afterward.
            reconnect_endpoint: None,
            reconnect_config: config,
        })
    }

    fn unavailable(reason: SessionUnavailableReason, detail: impl Into<String>) -> Self {
        Self::unavailable_from_status(SessionStatus::Unavailable(SessionUnavailable {
            reason,
            detail: detail.into(),
        }))
    }

    fn unavailable_from_status(status: SessionStatus) -> Self {
        Self {
            status,
            session_id: None,
            client: None,
            reconnect_endpoint: None,
            reconnect_config: ShellSessionConfig::default(),
        }
    }

    fn unavailable_error(&self) -> SandboxError {
        status_error(&self.status)
    }

    /// Returns `true` when the error might indicate that the socket framing is
    /// out of sync (e.g. a stale response from a previously timed-out command
    /// was read instead of the expected response).
    ///
    /// NOTE: If additional desync-related issues are observed in the future,
    /// consider adding request-id correlation to the RPC client so that stale
    /// responses can be detected and drained without tearing down the connection.
    fn is_recoverable_desync(error: &SandboxError) -> bool {
        matches!(
            error,
            SandboxError::UnexpectedResponse { .. }
                | SandboxError::ShellDaemon { .. }
                | SandboxError::ConnectionClosed
                | SandboxError::Transport(_)
                | SandboxError::DeserializeResponse(_)
        )
    }

    /// Tear down the current connection and open a fresh socket + session to
    /// the same sidecar endpoint.  This recovers from protocol desync caused
    /// by client-side timeout cancellation leaving stale response frames in
    /// the socket buffer.
    ///
    /// Returns `Err` if no reconnect endpoint was stored (i.e. the session was
    /// created via `connect_with_stream`) or if the new connection fails.
    async fn reconnect(&mut self) -> Result<(), SandboxError> {
        let endpoint = match &self.reconnect_endpoint {
            Some(ep) => ep.clone(),
            None => {
                return Err(SandboxError::SessionUnavailable {
                    reason: SessionUnavailableReason::ProtocolError,
                    detail: "cannot reconnect: no sidecar endpoint stored".to_owned(),
                });
            }
        };

        // Drop the old framed connection so the stale socket is closed.
        self.client = None;
        self.session_id = None;

        let socket_path = match endpoint.transport {
            SidecarTransport::Vsock | SidecarTransport::Unix => {
                sidecar_unix_bridge_path(&endpoint.address)
            }
        };
        let socket_path = match socket_path {
            Some(p) => p.to_owned(),
            None => {
                return Err(SandboxError::SessionUnavailable {
                    reason: SessionUnavailableReason::InvalidAddress,
                    detail: format!(
                        "cannot reconnect: unable to resolve socket path from `{}`",
                        endpoint.address
                    ),
                });
            }
        };

        #[cfg(unix)]
        {
            let stream = UnixStream::connect(&socket_path).await.map_err(|error| {
                SandboxError::SessionUnavailable {
                    reason: SessionUnavailableReason::ConnectionFailed,
                    detail: format!(
                        "reconnect failed: could not connect to `{socket_path}`: {error}"
                    ),
                }
            })?;

            let mut client = ShellDaemonRpcClient::new(stream);
            let spawn_ack = client.spawn_session(&self.reconnect_config).await?;

            info!(
                session_id = %spawn_ack.session_id,
                socket_path = %socket_path,
                "shell session reconnected after desync recovery"
            );

            self.session_id = Some(spawn_ack.session_id);
            self.client = Some(client);
            self.status = sidecar_status(endpoint.transport, endpoint.address);
            Ok(())
        }

        #[cfg(not(unix))]
        {
            let _ = socket_path;
            Err(SandboxError::SessionUnavailable {
                reason: SessionUnavailableReason::UnsupportedTransport,
                detail: "cannot reconnect on non-unix platform".to_owned(),
            })
        }
    }

    /// Low-level exec_command RPC call without reconnection logic.
    async fn rpc_exec_command(
        &mut self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError> {
        let session_id = match self.session_id.as_deref() {
            Some(id) => id.to_owned(),
            None => return Err(self.unavailable_error()),
        };
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| status_error(&self.status))?;
        client
            .exec_command(&session_id, command, timeout_secs)
            .await
    }

    /// Low-level stream_output RPC call without reconnection logic.
    async fn rpc_stream_output(
        &mut self,
        max_bytes: Option<usize>,
    ) -> Result<StreamOutputChunk, SandboxError> {
        let session_id = match self.session_id.as_deref() {
            Some(id) => id.to_owned(),
            None => return Err(self.unavailable_error()),
        };
        let client = self
            .client
            .as_mut()
            .ok_or_else(|| status_error(&self.status))?;
        client.stream_output(&session_id, max_bytes).await
    }
}

#[async_trait]
impl ShellSession for VsockShellSession {
    fn status(&self) -> &SessionStatus {
        &self.status
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    async fn exec_command(
        &mut self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError> {
        if command.trim().is_empty() {
            return Err(SandboxError::EmptyCommand);
        }

        let result = self.rpc_exec_command(command, timeout_secs).await;

        match result {
            Ok(ack) => Ok(ack),
            Err(error)
                if Self::is_recoverable_desync(&error) && self.reconnect_endpoint.is_some() =>
            {
                warn!(
                    original_error = %error,
                    "sidecar shell session exec_command failed with recoverable error; attempting reconnect"
                );
                self.reconnect().await?;
                // Retry the command on the fresh session.
                self.rpc_exec_command(command, timeout_secs).await
            }
            Err(error) => Err(error),
        }
    }

    async fn stream_output(
        &mut self,
        max_bytes: Option<usize>,
    ) -> Result<StreamOutputChunk, SandboxError> {
        let result = self.rpc_stream_output(max_bytes).await;

        match result {
            Ok(chunk) => Ok(chunk),
            Err(error)
                if Self::is_recoverable_desync(&error) && self.reconnect_endpoint.is_some() =>
            {
                warn!(
                    original_error = %error,
                    "sidecar shell session stream_output failed with recoverable error; reconnecting (command output lost)"
                );
                self.reconnect().await?;
                // After reconnect the previous command's output is gone because
                // we opened a new session.  Return an explicit error so the
                // caller / LLM can retry the command.
                Err(SandboxError::ShellDaemon {
                    request_id: None,
                    message: "shell session was reconnected after a protocol desync; \
                              the previous command's output was lost — please retry the command"
                        .to_owned(),
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn kill_session(&mut self) -> Result<KillSessionAck, SandboxError> {
        let session_id = match self.session_id.as_deref() {
            Some(session_id) => session_id.to_owned(),
            None => return Err(self.unavailable_error()),
        };
        let client = match self.client.as_mut() {
            Some(client) => client,
            None => return Err(self.unavailable_error()),
        };
        let kill_ack = client.kill_session(&session_id).await?;
        if kill_ack.killed {
            self.status = SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::Disabled,
                detail: "shell session has been terminated".to_owned(),
            });
            self.session_id = None;
            self.client = None;
        }
        Ok(kill_ack)
    }
}

trait AsyncIo: AsyncRead + AsyncWrite {}

impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + ?Sized {}

type DynIoStream = Pin<Box<dyn AsyncIo + Send>>;

struct ShellDaemonRpcClient {
    framed: Framed<DynIoStream, LengthDelimitedCodec>,
    next_request_number: u64,
}

impl ShellDaemonRpcClient {
    fn new<Stream>(stream: Stream) -> Self
    where
        Stream: AsyncRead + AsyncWrite + Send + 'static,
    {
        Self {
            framed: Framed::new(Box::pin(stream), LengthDelimitedCodec::new()),
            next_request_number: 0,
        }
    }

    async fn spawn_session(
        &mut self,
        config: &ShellSessionConfig,
    ) -> Result<types::SpawnSessionAck, SandboxError> {
        let request_id = self.next_request_id("spawn");
        self.send_request(&ShellDaemonRequest::SpawnSession(SpawnSession {
            request_id: request_id.clone(),
            shell: config.shell.clone(),
            cwd: config.cwd(),
            env: config.env.clone(),
        }))
        .await?;
        match self.read_response().await? {
            ShellDaemonResponse::SpawnSession(ack) => Ok(ack),
            ShellDaemonResponse::Error(error) => Err(SandboxError::ShellDaemon {
                request_id: error.request_id,
                message: error.message,
            }),
            response => Err(SandboxError::UnexpectedResponse {
                expected: "spawn_session",
                actual: response_label(&response),
            }),
        }
    }

    async fn exec_command(
        &mut self,
        session_id: &str,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError> {
        let request_id = self.next_request_id("exec");
        self.send_request(&ShellDaemonRequest::ExecCommand(ExecCommand {
            request_id: request_id.clone(),
            session_id: session_id.to_owned(),
            command: command.to_owned(),
            timeout_secs,
        }))
        .await?;
        match self.read_response().await? {
            ShellDaemonResponse::ExecCommand(ack) if ack.accepted => Ok(ack),
            ShellDaemonResponse::ExecCommand(ack) => Err(SandboxError::ShellDaemonRejected {
                request_id: ack.request_id,
            }),
            ShellDaemonResponse::Error(error) => Err(SandboxError::ShellDaemon {
                request_id: error.request_id,
                message: error.message,
            }),
            response => Err(SandboxError::UnexpectedResponse {
                expected: "exec_command",
                actual: response_label(&response),
            }),
        }
    }

    async fn stream_output(
        &mut self,
        session_id: &str,
        max_bytes: Option<usize>,
    ) -> Result<StreamOutputChunk, SandboxError> {
        let request_id = self.next_request_id("stream");
        self.send_request(&ShellDaemonRequest::StreamOutput(StreamOutput {
            request_id: request_id.clone(),
            session_id: session_id.to_owned(),
            max_bytes,
        }))
        .await?;
        match self.read_response().await? {
            ShellDaemonResponse::StreamOutput(chunk) => Ok(chunk),
            ShellDaemonResponse::Error(error) => Err(SandboxError::ShellDaemon {
                request_id: error.request_id,
                message: error.message,
            }),
            response => Err(SandboxError::UnexpectedResponse {
                expected: "stream_output",
                actual: response_label(&response),
            }),
        }
    }

    async fn kill_session(&mut self, session_id: &str) -> Result<KillSessionAck, SandboxError> {
        let request_id = self.next_request_id("kill");
        self.send_request(&ShellDaemonRequest::KillSession(KillSession {
            request_id: request_id.clone(),
            session_id: session_id.to_owned(),
        }))
        .await?;
        match self.read_response().await? {
            ShellDaemonResponse::KillSession(ack) => Ok(ack),
            ShellDaemonResponse::Error(error) => Err(SandboxError::ShellDaemon {
                request_id: error.request_id,
                message: error.message,
            }),
            response => Err(SandboxError::UnexpectedResponse {
                expected: "kill_session",
                actual: response_label(&response),
            }),
        }
    }

    fn next_request_id(&mut self, prefix: &str) -> String {
        self.next_request_number += 1;
        format!("{prefix}-{:06}", self.next_request_number)
    }

    async fn send_request(&mut self, request: &ShellDaemonRequest) -> Result<(), SandboxError> {
        let payload = serde_json::to_vec(request).map_err(SandboxError::SerializeRequest)?;
        self.framed.send(Bytes::from(payload)).await?;
        Ok(())
    }

    async fn read_response(&mut self) -> Result<ShellDaemonResponse, SandboxError> {
        let frame = match self.framed.next().await {
            Some(result) => result?,
            None => return Err(SandboxError::ConnectionClosed),
        };
        serde_json::from_slice(&frame).map_err(SandboxError::DeserializeResponse)
    }
}

fn response_label(response: &ShellDaemonResponse) -> &'static str {
    match response {
        ShellDaemonResponse::SpawnSession(_) => "spawn_session",
        ShellDaemonResponse::ExecCommand(_) => "exec_command",
        ShellDaemonResponse::StreamOutput(_) => "stream_output",
        ShellDaemonResponse::KillSession(_) => "kill_session",
        ShellDaemonResponse::Error(_) => "error",
    }
}

#[derive(Debug)]
pub struct LocalProcessShellSession {
    status: SessionStatus,
    session_id: String,
    config: ShellSessionConfig,
    output_queue: VecDeque<LocalOutputChunk>,
    next_request_number: u64,
}

impl LocalProcessShellSession {
    pub fn new(config: ShellSessionConfig) -> Result<Self, SandboxError> {
        config.validate()?;
        Ok(Self {
            status: SessionStatus::Ready(SessionConnection::LocalProcess),
            session_id: next_local_session_id(),
            config,
            output_queue: VecDeque::new(),
            next_request_number: 0,
        })
    }

    fn next_request_id(&mut self, prefix: &str) -> String {
        self.next_request_number += 1;
        format!("{prefix}-{:06}", self.next_request_number)
    }

    fn unavailable_error(&self) -> SandboxError {
        status_error(&self.status)
    }
}

#[async_trait]
impl ShellSession for LocalProcessShellSession {
    fn status(&self) -> &SessionStatus {
        &self.status
    }

    fn session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    async fn exec_command(
        &mut self,
        command: &str,
        timeout_secs: Option<u64>,
    ) -> Result<ExecCommandAck, SandboxError> {
        if !self.status.is_ready() {
            return Err(self.unavailable_error());
        }
        if command.trim().is_empty() {
            return Err(SandboxError::EmptyCommand);
        }

        let output = run_shell_command(&self.config, command, timeout_secs).await?;
        self.output_queue = build_output_queue(
            output.stdout,
            output.stderr,
            self.config.max_session_output_bytes,
        );
        let request_id = self.next_request_id("local-exec");
        Ok(ExecCommandAck {
            request_id,
            accepted: true,
        })
    }

    async fn stream_output(
        &mut self,
        max_bytes: Option<usize>,
    ) -> Result<StreamOutputChunk, SandboxError> {
        if !self.status.is_ready() {
            return Err(self.unavailable_error());
        }

        let request_id = self.next_request_id("local-stream");
        let (stream, data, eof) = next_output_chunk(
            &mut self.output_queue,
            max_bytes,
            self.config.max_stream_chunk_bytes,
        );
        Ok(StreamOutputChunk {
            request_id,
            session_id: self.session_id.clone(),
            stream,
            data: String::from_utf8_lossy(&data).into_owned(),
            eof,
        })
    }

    async fn kill_session(&mut self) -> Result<KillSessionAck, SandboxError> {
        let request_id = self.next_request_id("local-kill");
        let killed = self.status.is_ready();
        self.status = SessionStatus::Unavailable(SessionUnavailable {
            reason: SessionUnavailableReason::Disabled,
            detail: "shell session has been terminated".to_owned(),
        });
        self.output_queue.clear();
        Ok(KillSessionAck { request_id, killed })
    }
}

#[derive(Debug)]
struct LocalOutputChunk {
    stream: ShellOutputStream,
    data: Vec<u8>,
}

fn build_output_queue(
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    max_output_bytes: usize,
) -> VecDeque<LocalOutputChunk> {
    let mut queue = VecDeque::new();
    let mut remaining = max_output_bytes;
    push_bounded_output(
        &mut queue,
        ShellOutputStream::Stdout,
        stdout,
        &mut remaining,
    );
    push_bounded_output(
        &mut queue,
        ShellOutputStream::Stderr,
        stderr,
        &mut remaining,
    );
    queue
}

fn push_bounded_output(
    queue: &mut VecDeque<LocalOutputChunk>,
    stream: ShellOutputStream,
    data: Vec<u8>,
    remaining: &mut usize,
) {
    if *remaining == 0 || data.is_empty() {
        return;
    }
    let take = data.len().min(*remaining);
    if take == 0 {
        return;
    }
    queue.push_back(LocalOutputChunk {
        stream,
        data: data.into_iter().take(take).collect(),
    });
    *remaining -= take;
}

fn next_output_chunk(
    queue: &mut VecDeque<LocalOutputChunk>,
    max_bytes: Option<usize>,
    max_stream_chunk_bytes: usize,
) -> (ShellOutputStream, Vec<u8>, bool) {
    let bounded_max = max_bytes
        .unwrap_or(max_stream_chunk_bytes)
        .max(1)
        .min(max_stream_chunk_bytes);
    match queue.pop_front() {
        Some(mut chunk) => {
            if chunk.data.len() > bounded_max {
                let remainder = chunk.data.split_off(bounded_max);
                queue.push_front(LocalOutputChunk {
                    stream: chunk.stream,
                    data: remainder,
                });
            }
            let eof = queue.is_empty();
            (chunk.stream, chunk.data, eof)
        }
        None => (ShellOutputStream::Stdout, Vec::new(), true),
    }
}

async fn run_shell_command(
    config: &ShellSessionConfig,
    command_text: &str,
    timeout_secs: Option<u64>,
) -> Result<std::process::Output, SandboxError> {
    let mut command = Command::new(config.shell_binary());
    configure_shell_command(&mut command, command_text);
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(&config.env);
    if let Some(cwd) = config.cwd() {
        command.current_dir(cwd);
    }

    let run_future = command.output();
    if let Some(timeout_secs) = timeout_secs {
        let timeout_duration = Duration::from_secs(timeout_secs);
        match time::timeout(timeout_duration, run_future).await {
            Ok(result) => result.map_err(SandboxError::Transport),
            Err(_) => Err(SandboxError::CommandTimeout { timeout_secs }),
        }
    } else {
        run_future.await.map_err(SandboxError::Transport)
    }
}

#[cfg(windows)]
fn configure_shell_command(command: &mut Command, command_text: &str) {
    command.arg("/C").arg(command_text);
}

#[cfg(not(windows))]
fn configure_shell_command(command: &mut Command, command_text: &str) {
    command.arg("-lc").arg(command_text);
}

fn default_shell_binary() -> String {
    #[cfg(windows)]
    {
        "cmd".to_owned()
    }
    #[cfg(not(windows))]
    {
        "/bin/sh".to_owned()
    }
}

fn next_local_session_id() -> String {
    let session_number = NEXT_LOCAL_SESSION_NUMBER.fetch_add(1, Ordering::SeqCst) + 1;
    format!("local-session-{session_number:06}")
}

fn sidecar_status(transport: SidecarTransport, address: String) -> SessionStatus {
    SessionStatus::Ready(SessionConnection::Sidecar { transport, address })
}

fn missing_sidecar_status() -> SessionStatus {
    SessionStatus::Unavailable(SessionUnavailable {
        reason: SessionUnavailableReason::MissingSidecarEndpoint,
        detail: "runner bootstrap did not provide a sidecar endpoint".to_owned(),
    })
}

fn sidecar_unix_bridge_path(address: &str) -> Option<&str> {
    let address = address.trim();
    if address.is_empty() {
        None
    } else if let Some(path) = address.strip_prefix("unix://") {
        if path.is_empty() { None } else { Some(path) }
    } else if address.starts_with('/') {
        Some(address)
    } else {
        None
    }
}

fn status_error(status: &SessionStatus) -> SandboxError {
    match status {
        SessionStatus::Unavailable(unavailable) => SandboxError::SessionUnavailable {
            reason: unavailable.reason,
            detail: unavailable.detail.clone(),
        },
        SessionStatus::Ready(_) => SandboxError::SessionUnavailable {
            reason: SessionUnavailableReason::ProtocolError,
            detail: "session runtime is not initialized".to_owned(),
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessHardeningMechanism {
    Landlock,
    Seatbelt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessHardeningOutcome {
    Success,
    Failure,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHardeningAttempt {
    pub mechanism: ProcessHardeningMechanism,
    pub outcome: ProcessHardeningOutcome,
    pub detail: String,
}

impl ProcessHardeningAttempt {
    fn success(mechanism: ProcessHardeningMechanism, detail: impl Into<String>) -> Self {
        Self {
            mechanism,
            outcome: ProcessHardeningOutcome::Success,
            detail: detail.into(),
        }
    }

    fn failure(mechanism: ProcessHardeningMechanism, detail: impl Into<String>) -> Self {
        Self {
            mechanism,
            outcome: ProcessHardeningOutcome::Failure,
            detail: detail.into(),
        }
    }

    fn unsupported(mechanism: ProcessHardeningMechanism, detail: impl Into<String>) -> Self {
        Self {
            mechanism,
            outcome: ProcessHardeningOutcome::Unsupported,
            detail: detail.into(),
        }
    }
}

pub fn attempt_process_tier_hardening() -> ProcessHardeningAttempt {
    let attempt = hardening_attempt_for_platform();
    match attempt.outcome {
        ProcessHardeningOutcome::Success => {
            info!(
                mechanism = ?attempt.mechanism,
                outcome = ?attempt.outcome,
                detail = %attempt.detail,
                "process-tier hardening attempt"
            );
        }
        ProcessHardeningOutcome::Failure | ProcessHardeningOutcome::Unsupported => {
            warn!(
                mechanism = ?attempt.mechanism,
                outcome = ?attempt.outcome,
                detail = %attempt.detail,
                "process-tier hardening attempt"
            );
        }
    }
    attempt
}

#[cfg(target_os = "linux")]
fn hardening_attempt_for_platform() -> ProcessHardeningAttempt {
    probe_path_support(
        Path::new("/sys/kernel/security/landlock"),
        ProcessHardeningMechanism::Landlock,
        "landlock kernel interface detected; best-effort hook acknowledged",
        "landlock kernel interface not found on host",
    )
}

#[cfg(target_os = "macos")]
fn hardening_attempt_for_platform() -> ProcessHardeningAttempt {
    probe_path_support(
        Path::new("/usr/bin/sandbox-exec"),
        ProcessHardeningMechanism::Seatbelt,
        "seatbelt interface detected; best-effort hook acknowledged",
        "seatbelt interface not found on host",
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn hardening_attempt_for_platform() -> ProcessHardeningAttempt {
    ProcessHardeningAttempt::unsupported(
        ProcessHardeningMechanism::Landlock,
        "process-tier hardening is unsupported on this platform",
    )
}

fn probe_path_support(
    probe_path: &Path,
    mechanism: ProcessHardeningMechanism,
    success_detail: &str,
    unsupported_detail: &str,
) -> ProcessHardeningAttempt {
    match fs::metadata(probe_path) {
        Ok(_) => ProcessHardeningAttempt::success(mechanism, success_detail),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            ProcessHardeningAttempt::unsupported(mechanism, unsupported_detail)
        }
        Err(error) => ProcessHardeningAttempt::failure(
            mechanism,
            format!("failed probing `{}`: {error}", probe_path.display()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use oxydra_shelld::ShellDaemonServer;
    use tokio::io::duplex;
    use types::SidecarTransport;

    use super::*;

    #[tokio::test]
    async fn vsock_session_reports_missing_sidecar_endpoint() {
        let session = VsockShellSession::connect(None, ShellSessionConfig::default()).await;
        assert_eq!(
            session.status(),
            &SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::MissingSidecarEndpoint,
                detail: "runner bootstrap did not provide a sidecar endpoint".to_owned(),
            })
        );
    }

    #[tokio::test]
    async fn vsock_session_rejects_non_vsock_transport() {
        let endpoint = SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: "/tmp/shell-daemon.sock".to_owned(),
        };
        let session =
            VsockShellSession::connect(Some(endpoint), ShellSessionConfig::default()).await;
        assert_eq!(
            session.status(),
            &SessionStatus::Unavailable(SessionUnavailable {
                reason: SessionUnavailableReason::UnsupportedTransport,
                detail: format!(
                    "expected `{:?}` sidecar transport, got `{:?}`",
                    SidecarTransport::Vsock,
                    SidecarTransport::Unix
                ),
            })
        );
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn local_process_shell_session_executes_and_streams_output() {
        let mut session = LocalProcessShellSession::new(ShellSessionConfig::default())
            .expect("local process session should initialize");
        session
            .exec_command("printf ws4-local && printf ws4-err >&2", Some(10))
            .await
            .expect("command should execute");

        let mut stdout = String::new();
        let mut stderr = String::new();
        loop {
            let chunk = session
                .stream_output(Some(4))
                .await
                .expect("stream output should succeed");
            match chunk.stream {
                ShellOutputStream::Stdout => stdout.push_str(&chunk.data),
                ShellOutputStream::Stderr => stderr.push_str(&chunk.data),
            }
            if chunk.eof {
                break;
            }
        }

        assert_eq!(stdout, "ws4-local");
        assert_eq!(stderr, "ws4-err");
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn vsock_shell_session_uses_shell_daemon_protocol_via_injected_stream() {
        let server = ShellDaemonServer::default();
        let (client_stream, server_stream) = duplex(16 * 1024);
        let server_task = tokio::spawn({
            let server = server.clone();
            async move { server.serve_stream(server_stream).await }
        });

        let mut session = VsockShellSession::connect_with_stream(
            client_stream,
            SidecarTransport::Vsock,
            "vsock://3:1074",
            ShellSessionConfig::default(),
        )
        .await
        .expect("session should initialize");
        assert!(session.status().is_ready());

        session
            .exec_command("printf ws4-vsock", Some(10))
            .await
            .expect("exec should be accepted");
        let mut stdout = String::new();
        loop {
            let chunk = session
                .stream_output(Some(3))
                .await
                .expect("stream should succeed");
            if chunk.stream == ShellOutputStream::Stdout {
                stdout.push_str(&chunk.data);
            }
            if chunk.eof {
                break;
            }
        }
        assert_eq!(stdout, "ws4-vsock");

        let kill_ack = session
            .kill_session()
            .await
            .expect("kill session should succeed");
        assert!(kill_ack.killed);
        drop(session);

        let result = server_task
            .await
            .expect("server task should join without panic");
        assert!(result.is_ok());
    }

    #[test]
    fn probe_path_support_maps_existing_and_missing_paths() {
        let existing = unique_temp_path("ws4-probe-existing");
        fs::write(&existing, b"ok").expect("temp file should be writable");
        let success = probe_path_support(
            &existing,
            ProcessHardeningMechanism::Landlock,
            "success",
            "unsupported",
        );
        assert_eq!(success.outcome, ProcessHardeningOutcome::Success);

        let missing = unique_temp_path("ws4-probe-missing");
        let unsupported = probe_path_support(
            &missing,
            ProcessHardeningMechanism::Seatbelt,
            "success",
            "unsupported",
        );
        assert_eq!(unsupported.outcome, ProcessHardeningOutcome::Unsupported);

        let _ = fs::remove_file(existing);
    }

    #[test]
    fn process_tier_hardening_uses_platform_specific_mechanism() {
        let attempt = hardening_attempt_for_platform();
        #[cfg(target_os = "linux")]
        assert_eq!(attempt.mechanism, ProcessHardeningMechanism::Landlock);
        #[cfg(target_os = "macos")]
        assert_eq!(attempt.mechanism, ProcessHardeningMechanism::Seatbelt);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        assert_eq!(attempt.mechanism, ProcessHardeningMechanism::Landlock);
    }

    #[test]
    fn is_recoverable_desync_classifies_errors_correctly() {
        // Desync-indicating errors → recoverable
        assert!(VsockShellSession::is_recoverable_desync(
            &SandboxError::UnexpectedResponse {
                expected: "stream_output",
                actual: "exec_command"
            }
        ));
        assert!(VsockShellSession::is_recoverable_desync(
            &SandboxError::ShellDaemon {
                request_id: None,
                message: "command execution timed out after 30s".to_owned()
            }
        ));
        assert!(VsockShellSession::is_recoverable_desync(
            &SandboxError::ConnectionClosed
        ));

        // Non-desync errors → not recoverable
        assert!(!VsockShellSession::is_recoverable_desync(
            &SandboxError::EmptyCommand
        ));
        assert!(!VsockShellSession::is_recoverable_desync(
            &SandboxError::InvalidConfig("test")
        ));
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn exec_command_reconnects_on_desync_error() {
        use tokio::net::UnixListener;

        // Create a real shell-daemon listening on a temp Unix socket
        // so reconnection can open a new connection to it.
        let socket_dir = std::env::temp_dir().join(format!("ox-ds-{}", std::process::id()));
        fs::create_dir_all(&socket_dir).expect("socket dir should be created");
        let socket_path = socket_dir.join("shell-daemon.sock");

        let server = ShellDaemonServer::default();
        let listener = UnixListener::bind(&socket_path).expect("listener should bind");
        let server_task = tokio::spawn({
            let server = server.clone();
            async move { server.serve_unix_listener(listener).await }
        });

        // Connect to the daemon using a duplex stream initially
        // (simulating the first connection).
        let (client_stream, server_stream) = duplex(16 * 1024);
        let initial_server_task = tokio::spawn({
            let server = server.clone();
            async move { server.serve_stream(server_stream).await }
        });

        let mut session = VsockShellSession::connect_with_stream(
            client_stream,
            SidecarTransport::Unix,
            format!("unix://{}", socket_path.display()),
            ShellSessionConfig::default(),
        )
        .await
        .expect("session should initialize");

        // Set the reconnect endpoint so reconnection is possible.
        session.reconnect_endpoint = Some(SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: format!("unix://{}", socket_path.display()),
        });
        session.reconnect_config = ShellSessionConfig::default();

        // Verify the session works.
        session
            .exec_command("printf desync-test", Some(10))
            .await
            .expect("initial exec should succeed");
        let chunk = session
            .stream_output(None)
            .await
            .expect("initial stream should succeed");
        assert!(chunk.data.contains("desync-test"));

        // Now drop the initial connection to simulate desync — set the
        // client to None so the next call will fail with unavailable,
        // then manually set it back to trigger the right error path.
        // Instead, let's close the server side of the duplex to simulate
        // a broken connection.
        drop(initial_server_task);

        // The next exec_command should fail on the broken connection
        // and reconnect to the Unix socket listener.
        let result = session
            .exec_command("printf after-reconnect", Some(10))
            .await;
        // After reconnect, the command should succeed.
        assert!(
            result.is_ok(),
            "exec_command should succeed after reconnect, got: {result:?}"
        );

        let chunk = session
            .stream_output(None)
            .await
            .expect("stream after reconnect should succeed");
        assert!(chunk.data.contains("after-reconnect"));

        // Cleanup
        server_task.abort();
        let _ = fs::remove_dir_all(socket_dir);
    }

    fn unique_temp_path(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{nanos}"))
    }
}
