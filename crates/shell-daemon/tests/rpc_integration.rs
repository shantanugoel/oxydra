#![cfg(not(windows))]

use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use shell_daemon::{ShellDaemonConfig, ShellDaemonServer};
use tokio::io::duplex;
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use types::{
    ExecCommand, KillSession, ShellDaemonRequest, ShellDaemonResponse, SpawnSession, StreamOutput,
};

#[tokio::test]
async fn command_execution_returns_stdout_over_protocol() {
    let server = ShellDaemonServer::default();
    let (client_stream, server_stream) = duplex(16 * 1024);
    let server_task = tokio::spawn({
        let server = server.clone();
        async move { server.serve_stream(server_stream).await }
    });

    let mut client = Framed::new(client_stream, LengthDelimitedCodec::new());
    let session_id = spawn_session(&mut client, "spawn-1").await;

    exec_command(&mut client, "exec-echo", &session_id, "echo ws3-echo").await;
    let echo_output = collect_stdout(&mut client, &session_id, "stream-echo", 128).await;
    assert_eq!(echo_output.trim_end_matches(['\r', '\n']), "ws3-echo");

    exec_command(&mut client, "exec-printf", &session_id, "printf ws3-printf").await;
    let printf_output = collect_stdout(&mut client, &session_id, "stream-printf", 128).await;
    assert_eq!(printf_output, "ws3-printf");

    send_request(
        &mut client,
        &ShellDaemonRequest::KillSession(KillSession {
            request_id: "kill-1".to_owned(),
            session_id,
        }),
    )
    .await;
    match read_response(&mut client).await {
        ShellDaemonResponse::KillSession(ack) => assert!(ack.killed),
        response => panic!("expected kill_session response, got {response:?}"),
    }

    drop(client);
    let result = server_task
        .await
        .expect("server task should join without panic");
    assert!(result.is_ok());
}

#[tokio::test]
async fn deterministic_session_ids_and_bounded_streaming_are_enforced() {
    let server = ShellDaemonServer::new(ShellDaemonConfig {
        max_session_output_bytes: 1024,
        max_stream_chunk_bytes: 3,
    })
    .expect("server config should be valid");
    let (client_stream, server_stream) = duplex(16 * 1024);
    let server_task = tokio::spawn({
        let server = server.clone();
        async move { server.serve_stream(server_stream).await }
    });

    let mut client = Framed::new(client_stream, LengthDelimitedCodec::new());
    let session_one = spawn_session(&mut client, "spawn-1").await;
    let session_two = spawn_session(&mut client, "spawn-2").await;
    assert_eq!(session_one, "session-000001");
    assert_eq!(session_two, "session-000002");

    exec_command(&mut client, "exec-bounded", &session_one, "printf abcdef").await;
    send_request(
        &mut client,
        &ShellDaemonRequest::StreamOutput(StreamOutput {
            request_id: "stream-bounded-1".to_owned(),
            session_id: session_one.clone(),
            max_bytes: Some(20),
        }),
    )
    .await;
    match read_response(&mut client).await {
        ShellDaemonResponse::StreamOutput(chunk) => {
            assert_eq!(chunk.data, "abc");
            assert!(!chunk.eof);
        }
        response => panic!("expected stream_output response, got {response:?}"),
    }

    send_request(
        &mut client,
        &ShellDaemonRequest::StreamOutput(StreamOutput {
            request_id: "stream-bounded-2".to_owned(),
            session_id: session_one,
            max_bytes: Some(20),
        }),
    )
    .await;
    match read_response(&mut client).await {
        ShellDaemonResponse::StreamOutput(chunk) => {
            assert_eq!(chunk.data, "def");
            assert!(chunk.eof);
        }
        response => panic!("expected stream_output response, got {response:?}"),
    }

    drop(client);
    let result = server_task
        .await
        .expect("server task should join without panic");
    assert!(result.is_ok());
}

async fn spawn_session(
    client: &mut Framed<tokio::io::DuplexStream, LengthDelimitedCodec>,
    request_id: &str,
) -> String {
    send_request(
        client,
        &ShellDaemonRequest::SpawnSession(SpawnSession {
            request_id: request_id.to_owned(),
            shell: None,
            cwd: None,
            env: BTreeMap::new(),
        }),
    )
    .await;
    match read_response(client).await {
        ShellDaemonResponse::SpawnSession(ack) => ack.session_id,
        response => panic!("expected spawn_session response, got {response:?}"),
    }
}

async fn exec_command(
    client: &mut Framed<tokio::io::DuplexStream, LengthDelimitedCodec>,
    request_id: &str,
    session_id: &str,
    command: &str,
) {
    send_request(
        client,
        &ShellDaemonRequest::ExecCommand(ExecCommand {
            request_id: request_id.to_owned(),
            session_id: session_id.to_owned(),
            command: command.to_owned(),
            timeout_secs: Some(10),
        }),
    )
    .await;
    match read_response(client).await {
        ShellDaemonResponse::ExecCommand(ack) => assert!(ack.accepted),
        response => panic!("expected exec_command response, got {response:?}"),
    }
}

async fn collect_stdout(
    client: &mut Framed<tokio::io::DuplexStream, LengthDelimitedCodec>,
    session_id: &str,
    request_prefix: &str,
    max_bytes: usize,
) -> String {
    let mut output = String::new();
    let mut iteration = 0_u64;
    loop {
        iteration += 1;
        send_request(
            client,
            &ShellDaemonRequest::StreamOutput(StreamOutput {
                request_id: format!("{request_prefix}-{iteration}"),
                session_id: session_id.to_owned(),
                max_bytes: Some(max_bytes),
            }),
        )
        .await;

        match read_response(client).await {
            ShellDaemonResponse::StreamOutput(chunk) => {
                output.push_str(&chunk.data);
                if chunk.eof {
                    return output;
                }
            }
            response => panic!("expected stream_output response, got {response:?}"),
        }
    }
}

async fn send_request(
    client: &mut Framed<tokio::io::DuplexStream, LengthDelimitedCodec>,
    request: &ShellDaemonRequest,
) {
    let payload = serde_json::to_vec(request).expect("request should serialize");
    client
        .send(Bytes::from(payload))
        .await
        .expect("request frame should send");
}

async fn read_response(
    client: &mut Framed<tokio::io::DuplexStream, LengthDelimitedCodec>,
) -> ShellDaemonResponse {
    let frame = client
        .next()
        .await
        .expect("response frame should exist")
        .expect("response frame should decode");
    serde_json::from_slice(&frame).expect("response should deserialize")
}
