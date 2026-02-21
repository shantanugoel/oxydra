use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io,
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::UnixListener,
    process::Command,
    sync::Mutex,
    time,
};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::warn;
use types::{
    ExecCommand, ExecCommandAck, KillSession, KillSessionAck, ShellDaemonError, ShellDaemonRequest,
    ShellDaemonResponse, ShellOutputStream, SpawnSession, SpawnSessionAck, StreamOutput,
    StreamOutputChunk,
};

pub const DEFAULT_MAX_SESSION_OUTPUT_BYTES: usize = 64 * 1024;
pub const DEFAULT_MAX_STREAM_CHUNK_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellDaemonConfig {
    pub max_session_output_bytes: usize,
    pub max_stream_chunk_bytes: usize,
}

impl Default for ShellDaemonConfig {
    fn default() -> Self {
        Self {
            max_session_output_bytes: DEFAULT_MAX_SESSION_OUTPUT_BYTES,
            max_stream_chunk_bytes: DEFAULT_MAX_STREAM_CHUNK_BYTES,
        }
    }
}

impl ShellDaemonConfig {
    fn validate(&self) -> Result<(), ShellDaemonServerError> {
        if self.max_session_output_bytes == 0 {
            return Err(ShellDaemonServerError::InvalidConfig(
                "max_session_output_bytes must be greater than zero",
            ));
        }
        if self.max_stream_chunk_bytes == 0 {
            return Err(ShellDaemonServerError::InvalidConfig(
                "max_stream_chunk_bytes must be greater than zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ShellDaemonServer {
    session_manager: Arc<Mutex<SessionManager>>,
}

impl Default for ShellDaemonServer {
    fn default() -> Self {
        Self::new(ShellDaemonConfig::default())
            .expect("default shell-daemon configuration must be valid")
    }
}

impl ShellDaemonServer {
    pub fn new(config: ShellDaemonConfig) -> Result<Self, ShellDaemonServerError> {
        config.validate()?;
        Ok(Self {
            session_manager: Arc::new(Mutex::new(SessionManager::new(config))),
        })
    }

    pub async fn serve_unix_listener(
        &self,
        listener: UnixListener,
    ) -> Result<(), ShellDaemonServerError> {
        loop {
            let (stream, _) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(error) = server.serve_stream(stream).await {
                    warn!(error = %error, "shell daemon stream terminated with error");
                }
            });
        }
    }

    pub async fn serve_stream<Stream>(&self, stream: Stream) -> Result<(), ShellDaemonServerError>
    where
        Stream: AsyncRead + AsyncWrite + Unpin,
    {
        let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
        while let Some(frame_result) = framed.next().await {
            let frame = frame_result?;
            let request = match serde_json::from_slice::<ShellDaemonRequest>(&frame) {
                Ok(request) => request,
                Err(source) => {
                    let response = ShellDaemonResponse::Error(ShellDaemonError {
                        request_id: None,
                        message: format!("invalid shell-daemon request frame: {source}"),
                    });
                    send_response(&mut framed, &response).await?;
                    continue;
                }
            };

            let response = self.handle_request(request).await;
            send_response(&mut framed, &response).await?;
        }

        Ok(())
    }

    pub async fn handle_request(&self, request: ShellDaemonRequest) -> ShellDaemonResponse {
        match request {
            ShellDaemonRequest::SpawnSession(request) => {
                let request_id = request.request_id.clone();
                match self.spawn_session(request).await {
                    Ok(ack) => ShellDaemonResponse::SpawnSession(ack),
                    Err(error) => to_error_response(Some(request_id), error),
                }
            }
            ShellDaemonRequest::ExecCommand(request) => {
                let request_id = request.request_id.clone();
                match self.exec_command(request).await {
                    Ok(ack) => ShellDaemonResponse::ExecCommand(ack),
                    Err(error) => to_error_response(Some(request_id), error),
                }
            }
            ShellDaemonRequest::StreamOutput(request) => {
                let request_id = request.request_id.clone();
                match self.stream_output(request).await {
                    Ok(chunk) => ShellDaemonResponse::StreamOutput(chunk),
                    Err(error) => to_error_response(Some(request_id), error),
                }
            }
            ShellDaemonRequest::KillSession(request) => {
                let request_id = request.request_id.clone();
                match self.kill_session(request).await {
                    Ok(ack) => ShellDaemonResponse::KillSession(ack),
                    Err(error) => to_error_response(Some(request_id), error),
                }
            }
        }
    }

    async fn spawn_session(&self, request: SpawnSession) -> Result<SpawnSessionAck, SessionError> {
        let mut manager = self.session_manager.lock().await;
        Ok(manager.spawn_session(request))
    }

    async fn exec_command(&self, request: ExecCommand) -> Result<ExecCommandAck, SessionError> {
        if request.command.trim().is_empty() {
            return Err(SessionError::EmptyCommand);
        }

        let session_runtime = {
            let manager = self.session_manager.lock().await;
            manager.session_runtime(&request.session_id)?
        };

        let output =
            run_shell_command(&session_runtime, &request.command, request.timeout_secs).await?;

        {
            let mut manager = self.session_manager.lock().await;
            manager.store_command_output(&request.session_id, output.stdout, output.stderr)?;
        }

        Ok(ExecCommandAck {
            request_id: request.request_id,
            accepted: true,
        })
    }

    async fn stream_output(
        &self,
        request: StreamOutput,
    ) -> Result<StreamOutputChunk, SessionError> {
        let mut manager = self.session_manager.lock().await;
        let (stream, data, eof) =
            manager.next_stream_chunk(&request.session_id, request.max_bytes)?;
        Ok(StreamOutputChunk {
            request_id: request.request_id,
            session_id: request.session_id,
            stream,
            data: String::from_utf8_lossy(&data).into_owned(),
            eof,
        })
    }

    async fn kill_session(&self, request: KillSession) -> Result<KillSessionAck, SessionError> {
        let mut manager = self.session_manager.lock().await;
        let killed = manager.kill_session(&request.session_id);
        Ok(KillSessionAck {
            request_id: request.request_id,
            killed,
        })
    }
}

#[derive(Debug, Error)]
pub enum ShellDaemonServerError {
    #[error("invalid shell daemon configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("shell daemon transport failure: {0}")]
    Transport(#[from] io::Error),
    #[error("shell daemon failed to encode response: {0}")]
    SerializeResponse(#[from] serde_json::Error),
}

#[derive(Debug, Error)]
enum SessionError {
    #[error("session `{session_id}` not found")]
    UnknownSession { session_id: String },
    #[error("command must not be empty")]
    EmptyCommand,
    #[error("command execution timed out after {timeout_secs}s")]
    CommandTimeout { timeout_secs: u64 },
    #[error("failed to execute command: {0}")]
    ExecuteCommand(#[source] io::Error),
}

#[derive(Debug, Clone)]
struct SessionRuntime {
    shell: String,
    cwd: Option<String>,
    env: BTreeMap<String, String>,
}

#[derive(Debug)]
struct Session {
    runtime: SessionRuntime,
    output_queue: VecDeque<OutputChunk>,
}

#[derive(Debug)]
struct OutputChunk {
    stream: ShellOutputStream,
    data: Vec<u8>,
}

#[derive(Debug)]
struct SessionManager {
    next_session_number: u64,
    max_session_output_bytes: usize,
    max_stream_chunk_bytes: usize,
    sessions: HashMap<String, Session>,
}

impl SessionManager {
    fn new(config: ShellDaemonConfig) -> Self {
        Self {
            next_session_number: 0,
            max_session_output_bytes: config.max_session_output_bytes,
            max_stream_chunk_bytes: config.max_stream_chunk_bytes,
            sessions: HashMap::new(),
        }
    }

    fn spawn_session(&mut self, request: SpawnSession) -> SpawnSessionAck {
        self.next_session_number += 1;
        let session_id = format!("session-{:06}", self.next_session_number);

        let shell = request
            .shell
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(default_shell_binary);
        let cwd = request.cwd.and_then(normalize_optional_value);

        self.sessions.insert(
            session_id.clone(),
            Session {
                runtime: SessionRuntime {
                    shell,
                    cwd,
                    env: request.env,
                },
                output_queue: VecDeque::new(),
            },
        );

        SpawnSessionAck {
            request_id: request.request_id,
            session_id,
        }
    }

    fn session_runtime(&self, session_id: &str) -> Result<SessionRuntime, SessionError> {
        self.sessions
            .get(session_id)
            .map(|session| session.runtime.clone())
            .ok_or_else(|| SessionError::UnknownSession {
                session_id: session_id.to_owned(),
            })
    }

    fn store_command_output(
        &mut self,
        session_id: &str,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    ) -> Result<(), SessionError> {
        let session =
            self.sessions
                .get_mut(session_id)
                .ok_or_else(|| SessionError::UnknownSession {
                    session_id: session_id.to_owned(),
                })?;
        session.output_queue = build_output_queue(stdout, stderr, self.max_session_output_bytes);
        Ok(())
    }

    fn next_stream_chunk(
        &mut self,
        session_id: &str,
        max_bytes: Option<usize>,
    ) -> Result<(ShellOutputStream, Vec<u8>, bool), SessionError> {
        let session =
            self.sessions
                .get_mut(session_id)
                .ok_or_else(|| SessionError::UnknownSession {
                    session_id: session_id.to_owned(),
                })?;
        let bounded_max = max_bytes
            .unwrap_or(self.max_stream_chunk_bytes)
            .max(1)
            .min(self.max_stream_chunk_bytes);

        match session.output_queue.pop_front() {
            Some(mut chunk) => {
                if chunk.data.len() > bounded_max {
                    let remainder = chunk.data.split_off(bounded_max);
                    session.output_queue.push_front(OutputChunk {
                        stream: chunk.stream,
                        data: remainder,
                    });
                }
                let eof = session.output_queue.is_empty();
                Ok((chunk.stream, chunk.data, eof))
            }
            None => Ok((ShellOutputStream::Stdout, Vec::new(), true)),
        }
    }

    fn kill_session(&mut self, session_id: &str) -> bool {
        self.sessions.remove(session_id).is_some()
    }
}

fn build_output_queue(
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    max_output_bytes: usize,
) -> VecDeque<OutputChunk> {
    let mut queue = VecDeque::new();
    let mut remaining = max_output_bytes;
    push_bounded_chunk(
        &mut queue,
        ShellOutputStream::Stdout,
        stdout,
        &mut remaining,
    );
    push_bounded_chunk(
        &mut queue,
        ShellOutputStream::Stderr,
        stderr,
        &mut remaining,
    );
    queue
}

fn push_bounded_chunk(
    queue: &mut VecDeque<OutputChunk>,
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
    queue.push_back(OutputChunk {
        stream,
        data: data.into_iter().take(take).collect(),
    });
    *remaining -= take;
}

fn normalize_optional_value(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
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

async fn run_shell_command(
    runtime: &SessionRuntime,
    command_text: &str,
    timeout_secs: Option<u64>,
) -> Result<std::process::Output, SessionError> {
    let mut command = Command::new(&runtime.shell);
    configure_shell_command(&mut command, command_text);
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &runtime.cwd {
        command.current_dir(cwd);
    }
    command.envs(&runtime.env);

    let run_future = command.output();
    if let Some(timeout_secs) = timeout_secs {
        let timeout_duration = Duration::from_secs(timeout_secs);
        match time::timeout(timeout_duration, run_future).await {
            Ok(result) => result.map_err(SessionError::ExecuteCommand),
            Err(_) => Err(SessionError::CommandTimeout { timeout_secs }),
        }
    } else {
        run_future.await.map_err(SessionError::ExecuteCommand)
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

async fn send_response<Stream>(
    framed: &mut Framed<Stream, LengthDelimitedCodec>,
    response: &ShellDaemonResponse,
) -> Result<(), ShellDaemonServerError>
where
    Stream: AsyncRead + AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(response)?;
    framed.send(Bytes::from(payload)).await?;
    Ok(())
}

fn to_error_response(request_id: Option<String>, error: SessionError) -> ShellDaemonResponse {
    ShellDaemonResponse::Error(ShellDaemonError {
        request_id,
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_deterministic() {
        let mut manager = SessionManager::new(ShellDaemonConfig::default());
        let first = manager.spawn_session(SpawnSession {
            request_id: "req-1".to_owned(),
            shell: None,
            cwd: None,
            env: BTreeMap::new(),
        });
        let second = manager.spawn_session(SpawnSession {
            request_id: "req-2".to_owned(),
            shell: None,
            cwd: None,
            env: BTreeMap::new(),
        });

        assert_eq!(first.session_id, "session-000001");
        assert_eq!(second.session_id, "session-000002");
    }

    #[test]
    fn output_queue_respects_max_output_limit() {
        let queue = build_output_queue(b"abcdef".to_vec(), b"stderr".to_vec(), 5);

        assert_eq!(queue.len(), 1);
        let first = queue.front().expect("chunk should exist");
        assert_eq!(first.stream, ShellOutputStream::Stdout);
        assert_eq!(first.data, b"abcde");
    }
}
