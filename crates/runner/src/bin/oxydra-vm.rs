use std::{
    fs,
    io::{self, Read},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use clap::Parser;
use gateway::{GatewayServer, RuntimeGatewayTurnRunner};
use runner::{BootstrapError, CliOverrides, bootstrap_vm_runtime};
use runtime::AgentRuntime;
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{info, warn};
use types::init_tracing;

const DEFAULT_GATEWAY_BIND_ADDRESS: &str = "127.0.0.1:0";
const GATEWAY_ENDPOINT_MARKER_FILE: &str = "gateway-endpoint";

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "oxydra-vm", about = "Oxydra VM process runtime")]
struct OxydraVmArgs {
    #[arg(long = "user-id")]
    user_id: String,
    #[arg(long = "workspace-root")]
    workspace_root: PathBuf,
    #[arg(long = "bootstrap-stdin")]
    bootstrap_stdin: bool,
    #[arg(long = "bootstrap-file", env = "OXYDRA_BOOTSTRAP_FILE")]
    bootstrap_file: Option<PathBuf>,
    #[arg(long = "gateway-bind", default_value = DEFAULT_GATEWAY_BIND_ADDRESS)]
    gateway_bind: String,
}

#[derive(Debug, Error)]
enum VmError {
    #[error("user_id must not be empty")]
    InvalidUserId,
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
    #[error("failed to read bootstrap frame from stdin {stage}: {source}")]
    ReadBootstrapFrame {
        stage: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("failed to read bootstrap file `{path}`: {source}")]
    ReadBootstrapFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to bind gateway listener `{address}`: {source}")]
    BindGateway {
        address: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to resolve gateway listener local address: {0}")]
    GatewayAddress(#[source] io::Error),
    #[error("failed to write gateway endpoint marker `{path}`: {source}")]
    GatewayMarkerWrite {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("gateway server terminated: {0}")]
    ServeGateway(#[source] io::Error),
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = run().await {
        eprintln!("oxydra-vm error: {error}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<(), VmError> {
    init_tracing();
    let args = OxydraVmArgs::parse();
    if args.user_id.trim().is_empty() {
        return Err(VmError::InvalidUserId);
    }

    let bootstrap_frame = if args.bootstrap_stdin {
        Some(read_bootstrap_frame_from_stdin()?)
    } else if let Some(ref path) = args.bootstrap_file {
        Some(read_bootstrap_frame_from_file(path)?)
    } else {
        None
    };
    let bootstrap =
        bootstrap_vm_runtime(bootstrap_frame.as_deref(), None, CliOverrides::default()).await?;
    let provider_id = bootstrap.config.selection.provider.clone();
    let model_id = bootstrap.config.selection.model.clone();
    let startup_status = bootstrap.startup_status.clone();
    info!(
        provider = %provider_id,
        model = %model_id,
        "agent config loaded"
    );
    if startup_status.is_degraded() {
        warn!(
            user_id = %args.user_id,
            sandbox_tier = ?startup_status.sandbox_tier,
            sidecar_available = startup_status.sidecar_available,
            shell_available = startup_status.shell_available,
            browser_available = startup_status.browser_available,
            degraded_reasons = ?startup_status.degraded_reasons,
            "oxydra-vm startup status is degraded"
        );
    } else {
        info!(
            user_id = %args.user_id,
            sandbox_tier = ?startup_status.sandbox_tier,
            sidecar_available = startup_status.sidecar_available,
            shell_available = startup_status.shell_available,
            browser_available = startup_status.browser_available,
            "oxydra-vm startup status is ready"
        );
    }

    let mut runtime = AgentRuntime::new(
        bootstrap.provider,
        bootstrap.tool_registry,
        bootstrap.runtime_limits,
    );
    if let Some(memory) = bootstrap.memory {
        runtime = runtime.with_memory_retrieval(memory);
    }

    let turn_runner = Arc::new(
        RuntimeGatewayTurnRunner::new(Arc::new(runtime), provider_id, model_id),
    );
    let gateway = Arc::new(GatewayServer::with_startup_status(
        turn_runner,
        startup_status,
    ));
    let app = Arc::clone(&gateway).router();

    let listener = TcpListener::bind(&args.gateway_bind)
        .await
        .map_err(|source| VmError::BindGateway {
            address: args.gateway_bind.clone(),
            source,
        })?;
    let address = listener.local_addr().map_err(VmError::GatewayAddress)?;
    let marker_path = write_gateway_endpoint_marker(&args.workspace_root, address)?;
    info!(
        user_id = %args.user_id,
        gateway_endpoint = %gateway_endpoint(address),
        "oxydra-vm started"
    );

    // Run the gateway until the process receives a termination signal, then
    // clean up the endpoint marker so stale files do not confuse future
    // `connect_tui` calls.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(VmError::ServeGateway)?;

    let _ = fs::remove_file(&marker_path);
    Ok(())
}

fn write_gateway_endpoint_marker(
    workspace_root: &Path,
    address: SocketAddr,
) -> Result<PathBuf, VmError> {
    let marker_directory = workspace_root.join("ipc");
    fs::create_dir_all(&marker_directory).map_err(|source| VmError::GatewayMarkerWrite {
        path: marker_directory.clone(),
        source,
    })?;

    let marker_path = marker_directory.join(GATEWAY_ENDPOINT_MARKER_FILE);
    fs::write(&marker_path, gateway_endpoint(address)).map_err(|source| {
        VmError::GatewayMarkerWrite {
            path: marker_path.clone(),
            source,
        }
    })?;
    Ok(marker_path)
}

fn gateway_endpoint(address: SocketAddr) -> String {
    format!("ws://{address}/ws")
}

fn read_bootstrap_frame_from_file(path: &Path) -> Result<Vec<u8>, VmError> {
    let payload = fs::read(path).map_err(|source| VmError::ReadBootstrapFile {
        path: path.to_path_buf(),
        source,
    })?;
    // Wrap raw JSON in a 4-byte big-endian length prefix to match the frame
    // format that `bootstrap_vm_runtime` expects via `from_length_prefixed_json`.
    let len = u32::try_from(payload.len()).map_err(|_| VmError::ReadBootstrapFile {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidData, "bootstrap file too large"),
    })?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn read_bootstrap_frame_from_stdin() -> Result<Vec<u8>, VmError> {
    let mut stdin = io::stdin().lock();
    read_bootstrap_frame_from_reader(&mut stdin)
}

fn read_bootstrap_frame_from_reader(reader: &mut impl Read) -> Result<Vec<u8>, VmError> {
    let mut len_buf = [0_u8; 4];
    reader
        .read_exact(&mut len_buf)
        .map_err(|source| VmError::ReadBootstrapFrame {
            stage: "length prefix",
            source,
        })?;
    let payload_len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0_u8; payload_len];
    reader
        .read_exact(&mut payload)
        .map_err(|source| VmError::ReadBootstrapFrame {
            stage: "payload",
            source,
        })?;
    let mut frame = Vec::with_capacity(4 + payload_len);
    frame.extend_from_slice(&len_buf);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        io::Cursor,
        time::{SystemTime, UNIX_EPOCH},
    };

    use types::{RunnerBootstrapEnvelope, SandboxTier, SidecarEndpoint, SidecarTransport};

    use super::*;

    #[test]
    fn vm_args_parse_required_fields() {
        let args = OxydraVmArgs::try_parse_from([
            "oxydra-vm",
            "--user-id",
            "alice",
            "--workspace-root",
            "/tmp/workspace",
        ])
        .expect("args should parse");
        assert_eq!(args.user_id, "alice");
        assert_eq!(args.workspace_root, PathBuf::from("/tmp/workspace"));
        assert!(!args.bootstrap_stdin);
        assert_eq!(args.gateway_bind, DEFAULT_GATEWAY_BIND_ADDRESS);
    }

    #[test]
    fn vm_args_accept_bootstrap_file_flag() {
        let args = OxydraVmArgs::try_parse_from([
            "oxydra-vm",
            "--user-id",
            "alice",
            "--workspace-root",
            "/tmp/workspace",
            "--bootstrap-file",
            "/run/oxydra/bootstrap",
        ])
        .expect("args should parse with bootstrap file flag");
        assert_eq!(
            args.bootstrap_file,
            Some(PathBuf::from("/run/oxydra/bootstrap"))
        );
        assert!(!args.bootstrap_stdin);
    }

    #[test]
    fn bootstrap_frame_from_file_wraps_with_length_prefix() {
        let root = temp_dir("file-frame");
        let bootstrap = RunnerBootstrapEnvelope {
            user_id: "alice".to_owned(),
            sandbox_tier: SandboxTier::Container,
            workspace_root: "/tmp/oxydra/alice".to_owned(),
            sidecar_endpoint: None,
            runtime_policy: None,
            startup_status: None,
        };
        let json_bytes = serde_json::to_vec(&bootstrap).expect("should serialize");
        let file_path = root.join("bootstrap.json");
        fs::write(&file_path, &json_bytes).expect("should write test file");

        let frame = read_bootstrap_frame_from_file(&file_path).expect("should read bootstrap file");

        // First 4 bytes are big-endian length prefix.
        assert!(frame.len() >= 4);
        let len = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
        assert_eq!(len, json_bytes.len());
        assert_eq!(&frame[4..], &json_bytes);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn vm_args_accept_bootstrap_stdin_flag() {
        let args = OxydraVmArgs::try_parse_from([
            "oxydra-vm",
            "--user-id",
            "alice",
            "--workspace-root",
            "/tmp/workspace",
            "--bootstrap-stdin",
        ])
        .expect("args should parse with bootstrap stdin flag");
        assert!(args.bootstrap_stdin);
    }

    #[test]
    fn marker_writer_persists_gateway_endpoint() {
        let root = temp_dir("marker");
        let address: SocketAddr = "127.0.0.1:42001"
            .parse()
            .expect("socket address should parse");
        let marker_path =
            write_gateway_endpoint_marker(&root, address).expect("marker should write");
        assert!(
            marker_path.starts_with(root.join("ipc")),
            "marker file should be under workspace ipc/"
        );
        let marker = fs::read_to_string(&marker_path).expect("marker file should be readable");
        assert_eq!(marker, "ws://127.0.0.1:42001/ws");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bootstrap_frame_reader_preserves_length_prefixed_payload() {
        let encoded = RunnerBootstrapEnvelope {
            user_id: "alice".to_owned(),
            sandbox_tier: SandboxTier::Container,
            workspace_root: "/tmp/oxydra/alice".to_owned(),
            sidecar_endpoint: Some(SidecarEndpoint {
                transport: SidecarTransport::Unix,
                address: "/tmp/shell-daemon.sock".to_owned(),
            }),
            runtime_policy: None,
            startup_status: None,
        }
        .to_length_prefixed_json()
        .expect("bootstrap envelope should encode");
        let mut cursor = Cursor::new(encoded.clone());

        let frame = read_bootstrap_frame_from_reader(&mut cursor)
            .expect("frame reader should return length-prefixed bytes");
        assert_eq!(frame, encoded);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        path.push(format!(
            "oxydra-vm-test-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }
}
