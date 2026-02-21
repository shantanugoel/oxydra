use std::{
    fs, io,
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use clap::Parser;
use gateway::{GatewayServer, RuntimeGatewayTurnRunner};
use runtime::AgentRuntime;
use thiserror::Error;
use tokio::net::TcpListener;
use tui::{CliError as BootstrapError, CliOverrides, bootstrap_vm_runtime};

const DEFAULT_GATEWAY_BIND_ADDRESS: &str = "127.0.0.1:0";
const GATEWAY_ENDPOINT_MARKER_FILE: &str = "gateway-endpoint";

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "oxydra-vm", about = "Oxydra VM process runtime")]
struct OxydraVmArgs {
    #[arg(long = "user-id")]
    user_id: String,
    #[arg(long = "workspace-root")]
    workspace_root: PathBuf,
    #[arg(long = "gateway-bind", default_value = DEFAULT_GATEWAY_BIND_ADDRESS)]
    gateway_bind: String,
}

#[derive(Debug, Error)]
enum VmError {
    #[error("user_id must not be empty")]
    InvalidUserId,
    #[error(transparent)]
    Bootstrap(#[from] BootstrapError),
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
    let args = OxydraVmArgs::parse();
    if args.user_id.trim().is_empty() {
        return Err(VmError::InvalidUserId);
    }

    let bootstrap = bootstrap_vm_runtime(None, None, CliOverrides::default()).await?;
    let provider_id = bootstrap.config.selection.provider.clone();
    let model_id = bootstrap.config.selection.model.clone();

    let mut runtime = AgentRuntime::new(
        bootstrap.provider,
        bootstrap.tool_registry,
        bootstrap.runtime_limits,
    );
    if let Some(memory) = bootstrap.memory {
        runtime = runtime.with_memory(memory);
    }

    let turn_runner = Arc::new(RuntimeGatewayTurnRunner::new(
        Arc::new(runtime),
        provider_id,
        model_id,
    ));
    let gateway = Arc::new(GatewayServer::new(turn_runner));
    let app = Arc::clone(&gateway).router();

    let listener = TcpListener::bind(&args.gateway_bind)
        .await
        .map_err(|source| VmError::BindGateway {
            address: args.gateway_bind.clone(),
            source,
        })?;
    let address = listener.local_addr().map_err(VmError::GatewayAddress)?;
    write_gateway_endpoint_marker(&args.workspace_root, address)?;
    eprintln!(
        "oxydra-vm started user_id={} gateway_endpoint={}",
        args.user_id,
        gateway_endpoint(address)
    );

    axum::serve(listener, app)
        .await
        .map_err(VmError::ServeGateway)
}

fn write_gateway_endpoint_marker(
    workspace_root: &Path,
    address: SocketAddr,
) -> Result<(), VmError> {
    let marker_directory = workspace_root.join("tmp");
    fs::create_dir_all(&marker_directory).map_err(|source| VmError::GatewayMarkerWrite {
        path: marker_directory.clone(),
        source,
    })?;

    let marker_path = marker_directory.join(GATEWAY_ENDPOINT_MARKER_FILE);
    fs::write(&marker_path, gateway_endpoint(address)).map_err(|source| {
        VmError::GatewayMarkerWrite {
            path: marker_path,
            source,
        }
    })
}

fn gateway_endpoint(address: SocketAddr) -> String {
    format!("ws://{address}/ws")
}

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        time::{SystemTime, UNIX_EPOCH},
    };

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
        assert_eq!(args.gateway_bind, DEFAULT_GATEWAY_BIND_ADDRESS);
    }

    #[test]
    fn marker_writer_persists_gateway_endpoint() {
        let root = temp_dir("marker");
        let address: SocketAddr = "127.0.0.1:42001"
            .parse()
            .expect("socket address should parse");
        write_gateway_endpoint_marker(&root, address).expect("marker should write");
        let marker_path = root.join("tmp").join(GATEWAY_ENDPOINT_MARKER_FILE);
        let marker = fs::read_to_string(marker_path).expect("marker file should be readable");
        assert_eq!(marker, "ws://127.0.0.1:42001/ws");
        let _ = fs::remove_dir_all(root);
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
