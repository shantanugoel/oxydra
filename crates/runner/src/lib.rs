use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use bollard::{
    API_DEFAULT_VERSION, Docker,
    errors::Error as BollardError,
    models::{ContainerCreateBody, HostConfig, RestartPolicy, RestartPolicyNameEnum},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder, RemoveContainerOptionsBuilder,
        StopContainerOptionsBuilder,
    },
};
use futures_util::{SinkExt, StreamExt, TryStreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, Uri, body::Bytes};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as HyperlocalUri};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};
use tools::{ProcessHardeningOutcome, attempt_process_tier_hardening};
use tracing::{info, warn};
use types::{
    BootstrapEnvelopeError, GATEWAY_PROTOCOL_VERSION, GatewayClientFrame, GatewayClientHello,
    GatewayHealthCheck, GatewayServerFrame, RunnerBootstrapEnvelope, RunnerConfigError,
    RunnerControl, RunnerControlError, RunnerControlErrorCode, RunnerControlHealthStatus,
    RunnerControlResponse, RunnerControlShutdownStatus, RunnerGlobalConfig, RunnerGuestImages,
    RunnerMountPaths, RunnerResolvedMountPaths, RunnerResourceLimits, RunnerRuntimePolicy,
    RunnerUserConfig, RunnerUserRegistration, SandboxTier, SidecarEndpoint, SidecarTransport,
    StartupDegradedReason, StartupDegradedReasonCode, StartupStatusReport,
};

mod backend;
pub mod bootstrap;
pub mod catalog;

pub use bootstrap::{
    BootstrapError, CliOverrides, VmBootstrapRuntime, bootstrap_vm_runtime,
    bootstrap_vm_runtime_with_paths, build_memory_backend, build_provider, build_reliable_provider,
    load_agent_config, load_agent_config_with_paths, resolve_model_catalog, runtime_limits,
};

#[cfg(test)]
mod tests;

pub const PROCESS_TIER_WARNING: &str = "Process tier is insecure: isolation is degraded and not production-safe; shell/browser tools are disabled.";

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";
const LOGS_DIR_NAME: &str = "logs";
const IPC_DIR_NAME: &str = "ipc";
const INTERNAL_DIR_NAME: &str = ".oxydra";
pub const GATEWAY_ENDPOINT_MARKER_FILE: &str = "gateway-endpoint";
pub const RUNNER_CONTROL_SOCKET_NAME: &str = "runner-control.sock";

const FIRECRACKER_BINARY: &str = "firecracker";
const PROCESS_EXECUTABLE_ENV_KEY: &str = "OXYDRA_VM_PROCESS_EXECUTABLE";
const DEFAULT_PROCESS_EXECUTABLE: &str = "oxydra-vm";
const DEFAULT_DOCKER_TIMEOUT_SECS: u64 = 120;

const DOCKER_SANDBOXD_SOCKET_RELATIVE_PATH: &str = ".docker/sandboxes/sandboxd.sock";
const DOCKER_SANDBOX_VM_ENDPOINT: &str = "/vm";
const FIRECRACKER_API_READY_TIMEOUT: Duration = Duration::from_secs(5);
const FIRECRACKER_API_READY_POLL_INTERVAL: Duration = Duration::from_millis(150);
const RUNNER_CONTROL_MAX_FRAME_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct Runner {
    global_config: RunnerGlobalConfig,
    global_config_path: PathBuf,
    backend: Arc<dyn SandboxBackend>,
}

impl Runner {
    pub fn from_global_config_path(path: impl AsRef<Path>) -> Result<Self, RunnerError> {
        Self::from_global_config_path_with_backend(path, Arc::new(CrateSandboxBackend))
    }

    pub fn from_global_config_path_with_backend(
        path: impl AsRef<Path>,
        backend: Arc<dyn SandboxBackend>,
    ) -> Result<Self, RunnerError> {
        let global_config_path = path.as_ref().to_path_buf();
        let global_config = load_runner_global_config(&global_config_path)?;
        Ok(Self {
            global_config,
            global_config_path,
            backend,
        })
    }

    pub fn global_config(&self) -> &RunnerGlobalConfig {
        &self.global_config
    }

    pub fn load_user_config(&self, user_id: &str) -> Result<RunnerUserConfig, RunnerError> {
        let user_id = validate_user_id(user_id)?;
        let registration = self.user_registration(user_id)?;
        let user_config_path = self.resolve_user_config_path(registration);
        load_runner_user_config(&user_config_path)
    }

    pub fn provision_user_workspace(&self, user_id: &str) -> Result<UserWorkspace, RunnerError> {
        let user_id = validate_user_id(user_id)?;
        provision_user_workspace(self.resolved_workspace_root(), user_id)
    }

    pub fn start_user(&self, request: RunnerStartRequest) -> Result<RunnerStartup, RunnerError> {
        self.start_user_for_host(request, std::env::consts::OS)
    }

    pub fn start_user_for_host(
        &self,
        request: RunnerStartRequest,
        host_os: &str,
    ) -> Result<RunnerStartup, RunnerError> {
        let user_id = validate_user_id(&request.user_id)?.to_owned();
        let user_config = self.load_user_config(&user_id)?;
        let workspace = self.provision_user_workspace(&user_id)?;
        let effective_launch_settings =
            resolve_effective_launch_settings(&workspace, &user_config)?;
        let sandbox_tier =
            resolve_sandbox_tier(&self.global_config, &user_config, request.insecure);
        let capabilities = resolve_requested_capabilities(sandbox_tier, &user_config);

        // Pre-compute the sidecar endpoint so we can build the bootstrap envelope
        // before launching the backend. Container/MicroVM tiers need the bootstrap
        // file on disk before the guest process starts.
        let pre_sidecar_endpoint = if capabilities.shell || capabilities.browser {
            Some(pre_compute_sidecar_endpoint(
                sandbox_tier,
                host_os,
                &workspace,
            ))
        } else {
            None
        };

        let pre_startup_status = build_startup_status_report(
            sandbox_tier,
            capabilities,
            // Optimistically assume sidecar will launch for the bootstrap envelope.
            // If it fails, the guest reads a slightly stale `sidecar_available` but
            // the control plane will report the real value at health-check time.
            capabilities.shell && pre_sidecar_endpoint.is_some(),
            capabilities.browser && pre_sidecar_endpoint.is_some(),
            pre_sidecar_endpoint.is_some(),
            &[], // degraded reasons are unknown before launch
        );

        let bootstrap = RunnerBootstrapEnvelope {
            user_id: user_id.clone(),
            sandbox_tier,
            workspace_root: workspace.root.to_string_lossy().into_owned(),
            sidecar_endpoint: pre_sidecar_endpoint,
            runtime_policy: Some(effective_launch_settings.runtime_policy()),
            startup_status: Some(pre_startup_status),
        };
        bootstrap.validate()?;

        // For non-Process tiers, write the bootstrap to a file that the guest
        // can read (mounted into the container or injected into boot_args).
        let bootstrap_file = if sandbox_tier != SandboxTier::Process {
            Some(write_bootstrap_file(&workspace, &bootstrap)?)
        } else {
            None
        };

        // Copy host agent config into the workspace internal directory so the
        // guest container can discover it. Also collect env vars referenced by
        // the config (api_key_env, etc.) to forward into the oxydra-vm container.
        // Shell-vm gets a separate set of env vars to avoid leaking API keys.
        let (mut extra_env, mut shell_env) = if sandbox_tier != SandboxTier::Process {
            copy_agent_config_to_workspace(&workspace)?;
            let config_env = collect_config_env_vars();
            let shell_config_env = collect_shell_config_env_keys();
            (config_env, shell_config_env)
        } else {
            (Vec::new(), Vec::new())
        };

        // Split CLI/file env vars: SHELL_-prefixed entries go to shell-vm
        // (with the prefix stripped), everything else goes to oxydra-vm.
        for entry in request.extra_env {
            if let Some((key, value)) = entry.split_once('=') {
                if let Some(stripped) = key.strip_prefix("SHELL_") {
                    if !stripped.is_empty() {
                        shell_env.push(format!("{stripped}={value}"));
                    }
                } else {
                    extra_env.push(entry);
                }
            }
        }

        // Remove any stale gateway endpoint marker left by a previous run
        // so that the CLI (or any external poller) does not pick up the old
        // value before the freshly-launched guest has a chance to write the
        // new one.
        let stale_marker = workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE);
        let _ = fs::remove_file(&stale_marker);

        let mut launch = self.backend.launch(SandboxLaunchRequest {
            user_id: user_id.clone(),
            host_os: host_os.to_owned(),
            sandbox_tier,
            workspace: workspace.clone(),
            mounts: effective_launch_settings.mounts.clone(),
            resource_limits: effective_launch_settings.resources.clone(),
            credential_refs: effective_launch_settings.credential_refs.clone(),
            guest_images: self.global_config.guest_images.clone(),
            requested_shell: capabilities.shell,
            requested_browser: capabilities.browser,
            bootstrap_file,
            extra_env,
            shell_env,
        })?;

        let sidecar_endpoint = if launch.shell_available || launch.browser_available {
            launch.sidecar_endpoint.take()
        } else {
            None
        };
        let startup_status = build_startup_status_report(
            sandbox_tier,
            capabilities,
            launch.shell_available,
            launch.browser_available,
            sidecar_endpoint.is_some(),
            &launch.degraded_reasons,
        );

        // For Process tier, send bootstrap via stdin (existing behavior).
        if sandbox_tier == SandboxTier::Process {
            launch.launch.runtime.send_startup_bootstrap(&bootstrap)?;
        }

        info!(
            user_id = %user_id,
            sandbox_tier = ?sandbox_tier,
            sidecar_available = startup_status.sidecar_available,
            shell_available = startup_status.shell_available,
            browser_available = startup_status.browser_available,
            degraded_reasons = ?startup_status.degraded_reasons,
            "runner startup prepared"
        );
        for warning_message in &launch.warnings {
            warn!(
                user_id = %user_id,
                warning = %warning_message,
                "runner startup warning"
            );
        }

        Ok(RunnerStartup {
            user_id,
            sandbox_tier,
            workspace,
            shell_available: launch.shell_available,
            browser_available: launch.browser_available,
            startup_status,
            launch: launch.launch,
            bootstrap,
            warnings: launch.warnings,
            shutdown_complete: false,
        })
    }

    pub fn connect_tui(
        &self,
        request: RunnerTuiConnectRequest,
    ) -> Result<RunnerTuiConnection, RunnerError> {
        let user_id = validate_user_id(&request.user_id)?.to_owned();
        let _user_config = self.load_user_config(&user_id)?;
        let workspace = self.provision_user_workspace(&user_id)?;
        let endpoint_path = workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE);
        let gateway_endpoint = read_gateway_endpoint_marker(&endpoint_path, &user_id)?;
        let session_id =
            probe_gateway_health(&gateway_endpoint, &user_id).map_err(|error| {
                if let RunnerError::GatewayProbeFailed { endpoint, message } = error {
                    RunnerError::StaleGatewayEndpoint {
                        endpoint,
                        marker_path: endpoint_path.clone(),
                        message,
                    }
                } else {
                    error
                }
            })?;

        Ok(RunnerTuiConnection {
            user_id,
            workspace,
            gateway_endpoint,
            session_id,
        })
    }

    fn user_registration(&self, user_id: &str) -> Result<&RunnerUserRegistration, RunnerError> {
        self.global_config
            .users
            .get(user_id)
            .ok_or_else(|| RunnerError::UnknownUser {
                user_id: user_id.to_owned(),
            })
    }

    fn resolve_user_config_path(&self, registration: &RunnerUserRegistration) -> PathBuf {
        let configured_path = PathBuf::from(&registration.config_path);
        if configured_path.is_absolute() {
            configured_path
        } else {
            self.config_root_dir().join(configured_path)
        }
    }

    fn resolved_workspace_root(&self) -> PathBuf {
        let configured_root = PathBuf::from(&self.global_config.workspace_root);
        if configured_root.is_absolute() {
            configured_root
        } else {
            self.config_root_dir().join(configured_root)
        }
    }

    fn config_root_dir(&self) -> PathBuf {
        self.global_config_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }
}

pub fn load_runner_global_config(
    path: impl AsRef<Path>,
) -> Result<RunnerGlobalConfig, RunnerError> {
    let path = path.as_ref().to_path_buf();
    let contents = fs::read_to_string(&path).map_err(|source| RunnerError::ReadConfig {
        path: path.clone(),
        source,
    })?;
    let config: RunnerGlobalConfig =
        toml::from_str(&contents).map_err(|source| RunnerError::ParseGlobalConfig {
            path: path.clone(),
            source,
        })?;
    config.validate()?;
    Ok(config)
}

pub fn load_runner_user_config(path: impl AsRef<Path>) -> Result<RunnerUserConfig, RunnerError> {
    let path = path.as_ref().to_path_buf();
    let contents = fs::read_to_string(&path).map_err(|source| RunnerError::ReadConfig {
        path: path.clone(),
        source,
    })?;
    let config: RunnerUserConfig =
        toml::from_str(&contents).map_err(|source| RunnerError::ParseUserConfig {
            path: path.clone(),
            source,
        })?;
    config.validate()?;
    Ok(config)
}

pub fn provision_user_workspace(
    workspace_root: impl AsRef<Path>,
    user_id: &str,
) -> Result<UserWorkspace, RunnerError> {
    let user_id = validate_user_id(user_id)?;
    let root = workspace_root.as_ref().join(user_id);

    // Create directories first so canonicalize can resolve the path.
    for dir_name in [
        SHARED_DIR_NAME,
        TMP_DIR_NAME,
        VAULT_DIR_NAME,
        LOGS_DIR_NAME,
        IPC_DIR_NAME,
        INTERNAL_DIR_NAME,
    ] {
        let path = root.join(dir_name);
        fs::create_dir_all(&path)
            .map_err(|source| RunnerError::ProvisionWorkspace { path, source })?;
    }

    // Canonicalize the root to an absolute path so that Docker sandbox VM
    // file sharing directories and container bind mounts receive real paths.
    let root = fs::canonicalize(&root)
        .map_err(|source| RunnerError::ProvisionWorkspace { path: root, source })?;
    let shared = root.join(SHARED_DIR_NAME);
    let tmp = root.join(TMP_DIR_NAME);
    let vault = root.join(VAULT_DIR_NAME);
    let logs = root.join(LOGS_DIR_NAME);
    let ipc = root.join(IPC_DIR_NAME);
    let internal = root.join(INTERNAL_DIR_NAME);

    Ok(UserWorkspace {
        root,
        shared,
        tmp,
        vault,
        logs,
        ipc,
        internal,
    })
}

pub fn resolve_sandbox_tier(
    global_config: &RunnerGlobalConfig,
    user_config: &RunnerUserConfig,
    insecure: bool,
) -> SandboxTier {
    if insecure {
        SandboxTier::Process
    } else {
        user_config
            .behavior
            .sandbox_tier
            .unwrap_or(global_config.default_tier)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerGuestRole {
    OxydraVm,
    ShellVm,
}

impl RunnerGuestRole {
    fn as_label(self) -> &'static str {
        match self {
            Self::OxydraVm => "oxydra-vm",
            Self::ShellVm => "shell-vm",
        }
    }
}

impl std::fmt::Display for RunnerGuestRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerCommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl RunnerCommandSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
        }
    }
}

#[derive(Debug)]
pub struct RunnerGuestHandle {
    pub role: RunnerGuestRole,
    pub command: RunnerCommandSpec,
    pub pid: Option<u32>,
    lifecycle: RunnerGuestLifecycle,
    log_tasks: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Debug)]
enum RunnerGuestLifecycle {
    Process { child: Child },
    DockerContainer(DockerContainerHandle),
    Simulated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerContainerHandle {
    endpoint: DockerEndpoint,
    container_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DockerEndpoint {
    Local,
    UnixSocket(String),
}

impl DockerEndpoint {
    fn label(&self) -> String {
        match self {
            Self::Local => "local-docker-daemon".to_owned(),
            Self::UnixSocket(path) => format!("unix://{path}"),
        }
    }
}

impl RunnerGuestHandle {
    fn from_child(role: RunnerGuestRole, command: RunnerCommandSpec, child: Child) -> Self {
        Self {
            role,
            command,
            pid: Some(child.id()),
            lifecycle: RunnerGuestLifecycle::Process { child },
            log_tasks: Vec::new(),
        }
    }

    fn for_docker(
        role: RunnerGuestRole,
        command: RunnerCommandSpec,
        endpoint: DockerEndpoint,
        container_name: String,
    ) -> Self {
        Self {
            role,
            command,
            pid: None,
            lifecycle: RunnerGuestLifecycle::DockerContainer(DockerContainerHandle {
                endpoint,
                container_name,
            }),
            log_tasks: Vec::new(),
        }
    }

    pub fn simulated(role: RunnerGuestRole, command: RunnerCommandSpec) -> Self {
        Self {
            role,
            command,
            pid: None,
            lifecycle: RunnerGuestLifecycle::Simulated,
            log_tasks: Vec::new(),
        }
    }

    fn send_startup_bootstrap(
        &mut self,
        bootstrap: &RunnerBootstrapEnvelope,
    ) -> Result<(), RunnerError> {
        let frame = bootstrap.to_length_prefixed_json()?;
        self.send_startup_frame(&frame)
    }

    fn send_startup_frame(&mut self, frame: &[u8]) -> Result<(), RunnerError> {
        let RunnerGuestLifecycle::Process { child } = &mut self.lifecycle else {
            return Ok(());
        };
        let program = self.command.program.clone();
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| RunnerError::GuestLifecycle {
                action: "write_startup_frame",
                role: self.role,
                program: program.clone(),
                source: io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "startup stdin channel is not available",
                ),
            })?;
        stdin
            .write_all(frame)
            .map_err(|source| RunnerError::GuestLifecycle {
                action: "write_startup_frame",
                role: self.role,
                program: program.clone(),
                source,
            })?;
        stdin
            .flush()
            .map_err(|source| RunnerError::GuestLifecycle {
                action: "write_startup_frame",
                role: self.role,
                program,
                source,
            })?;
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), RunnerError> {
        let lifecycle = std::mem::replace(&mut self.lifecycle, RunnerGuestLifecycle::Simulated);
        match lifecycle {
            RunnerGuestLifecycle::Process { mut child } => {
                let program = self.command.program.clone();
                if child
                    .try_wait()
                    .map_err(|source| RunnerError::GuestLifecycle {
                        action: "check_status",
                        role: self.role,
                        program: program.clone(),
                        source,
                    })?
                    .is_none()
                {
                    child.kill().map_err(|source| RunnerError::GuestLifecycle {
                        action: "terminate",
                        role: self.role,
                        program: program.clone(),
                        source,
                    })?;
                    child.wait().map_err(|source| RunnerError::GuestLifecycle {
                        action: "wait",
                        role: self.role,
                        program,
                        source,
                    })?;
                }
            }
            RunnerGuestLifecycle::DockerContainer(handle) => {
                shutdown_docker_container(handle)?;
            }
            RunnerGuestLifecycle::Simulated => {}
        }

        // Join log pump threads with a timeout so we don't hang on shutdown.
        for handle in self.log_tasks.drain(..) {
            let _ = handle.join();
        }

        self.pid = None;
        Ok(())
    }

    /// Async variant of [`Self::shutdown`] for use inside an existing tokio
    /// runtime (e.g. the daemon control loop). Avoids nested `block_on` by
    /// calling async Docker APIs directly.
    pub async fn shutdown_async(&mut self) -> Result<(), RunnerError> {
        let lifecycle = std::mem::replace(&mut self.lifecycle, RunnerGuestLifecycle::Simulated);
        match lifecycle {
            RunnerGuestLifecycle::Process { mut child } => {
                let program = self.command.program.clone();
                if child
                    .try_wait()
                    .map_err(|source| RunnerError::GuestLifecycle {
                        action: "check_status",
                        role: self.role,
                        program: program.clone(),
                        source,
                    })?
                    .is_none()
                {
                    child.kill().map_err(|source| RunnerError::GuestLifecycle {
                        action: "terminate",
                        role: self.role,
                        program: program.clone(),
                        source,
                    })?;
                    child.wait().map_err(|source| RunnerError::GuestLifecycle {
                        action: "wait",
                        role: self.role,
                        program,
                        source,
                    })?;
                }
            }
            RunnerGuestLifecycle::DockerContainer(handle) => {
                shutdown_docker_container_async(handle).await?;
            }
            RunnerGuestLifecycle::Simulated => {}
        }

        for handle in self.log_tasks.drain(..) {
            let _ = handle.join();
        }

        self.pid = None;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RunnerLaunchHandle {
    pub tier: SandboxTier,
    pub runtime: RunnerGuestHandle,
    pub sidecar: Option<RunnerGuestHandle>,
    pub scope: Option<RunnerScopeHandle>,
}

impl RunnerLaunchHandle {
    pub fn shutdown(&mut self) -> Result<(), RunnerError> {
        let mut first_error = None;
        if let Some(sidecar) = self.sidecar.as_mut()
            && let Err(error) = sidecar.shutdown()
        {
            first_error = Some(error);
        }
        if let Err(error) = self.runtime.shutdown()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(scope) = self.scope.as_mut()
            && let Err(error) = scope.shutdown()
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// Async variant of [`Self::shutdown`] for use inside an existing tokio
    /// runtime.
    pub async fn shutdown_async(&mut self) -> Result<(), RunnerError> {
        let mut first_error = None;
        if let Some(sidecar) = self.sidecar.as_mut()
            && let Err(error) = sidecar.shutdown_async().await
        {
            first_error = Some(error);
        }
        if let Err(error) = self.runtime.shutdown_async().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if let Some(scope) = self.scope.as_mut()
            && let Err(error) = scope.shutdown_async().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerScopeHandle {
    DockerSandboxVm(DockerSandboxVmHandle),
    Simulated,
}

impl RunnerScopeHandle {
    fn shutdown(&mut self) -> Result<(), RunnerError> {
        match self {
            Self::DockerSandboxVm(vm_handle) => delete_docker_sandbox_vm_sync(
                vm_handle.sandbox_socket_path.clone(),
                vm_handle.vm_name.clone(),
            ),
            Self::Simulated => Ok(()),
        }
    }

    /// Async variant of [`Self::shutdown`] for use inside an existing tokio
    /// runtime.
    async fn shutdown_async(&mut self) -> Result<(), RunnerError> {
        match self {
            Self::DockerSandboxVm(vm_handle) => {
                delete_docker_sandbox_vm_async(
                    vm_handle.sandbox_socket_path.clone(),
                    vm_handle.vm_name.clone(),
                )
                .await
            }
            Self::Simulated => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerSandboxVmHandle {
    pub vm_name: String,
    pub sandbox_socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerStartRequest {
    pub user_id: String,
    pub insecure: bool,
    /// Extra environment variables to inject into the guest container.
    /// Each entry is `KEY=VALUE`. These are forwarded as-is and take
    /// precedence over auto-detected env vars from the agent config.
    pub extra_env: Vec<String>,
}

impl RunnerStartRequest {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            insecure: false,
            extra_env: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerTuiConnectRequest {
    pub user_id: String,
}

impl RunnerTuiConnectRequest {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerTuiConnection {
    pub user_id: String,
    pub workspace: UserWorkspace,
    pub gateway_endpoint: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserWorkspace {
    pub root: PathBuf,
    pub shared: PathBuf,
    pub tmp: PathBuf,
    pub vault: PathBuf,
    pub logs: PathBuf,
    pub ipc: PathBuf,
    /// Internal directory for oxydra-vm only (memory DB, config cache).
    /// Not accessible to LLM tools or shell-vm.
    pub internal: PathBuf,
}

impl UserWorkspace {
    /// Returns the path to the runner control socket in the IPC directory.
    pub fn control_socket_path(&self) -> PathBuf {
        self.ipc.join(RUNNER_CONTROL_SOCKET_NAME)
    }
}

#[derive(Debug)]
pub struct RunnerStartup {
    pub user_id: String,
    pub sandbox_tier: SandboxTier,
    pub workspace: UserWorkspace,
    pub shell_available: bool,
    pub browser_available: bool,
    pub startup_status: StartupStatusReport,
    pub launch: RunnerLaunchHandle,
    pub bootstrap: RunnerBootstrapEnvelope,
    pub warnings: Vec<String>,
    shutdown_complete: bool,
}

impl RunnerStartup {
    pub fn shutdown(&mut self) -> Result<(), RunnerError> {
        if self.shutdown_complete {
            return Ok(());
        }
        self.launch.shutdown()?;
        self.shutdown_complete = true;
        self.shell_available = false;
        self.browser_available = false;
        self.startup_status.shell_available = false;
        self.startup_status.browser_available = false;
        self.startup_status.sidecar_available = false;
        self.startup_status.push_reason(
            StartupDegradedReasonCode::RuntimeShutdown,
            "runtime is shut down",
        );
        // Remove the gateway endpoint marker so stale files don't confuse
        // a future `connect_tui` call.
        let marker_path = self.workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE);
        let _ = fs::remove_file(&marker_path);
        Ok(())
    }

    /// Async variant of [`Self::shutdown`] for use inside an existing tokio
    /// runtime (e.g. the daemon control loop).
    pub async fn shutdown_async(&mut self) -> Result<(), RunnerError> {
        if self.shutdown_complete {
            return Ok(());
        }
        self.launch.shutdown_async().await?;
        self.shutdown_complete = true;
        self.shell_available = false;
        self.browser_available = false;
        self.startup_status.shell_available = false;
        self.startup_status.browser_available = false;
        self.startup_status.sidecar_available = false;
        self.startup_status.push_reason(
            StartupDegradedReasonCode::RuntimeShutdown,
            "runtime is shut down",
        );
        let marker_path = self.workspace.ipc.join(GATEWAY_ENDPOINT_MARKER_FILE);
        let _ = fs::remove_file(&marker_path);
        Ok(())
    }

    pub fn handle_control(&mut self, request: RunnerControl) -> RunnerControlResponse {
        match request {
            RunnerControl::HealthCheck => {
                let startup_status = self.control_startup_status();
                let runtime_container_name = match &self.launch.runtime.lifecycle {
                    RunnerGuestLifecycle::DockerContainer(handle) => {
                        Some(handle.container_name.clone())
                    }
                    _ => None,
                };
                let status = RunnerControlHealthStatus {
                    user_id: self.user_id.clone(),
                    healthy: !self.shutdown_complete,
                    sandbox_tier: self.sandbox_tier,
                    startup_status: startup_status.clone(),
                    shell_available: startup_status.shell_available,
                    browser_available: startup_status.browser_available,
                    shutdown: self.shutdown_complete,
                    message: self.control_health_message(),
                    log_dir: Some(self.workspace.logs.to_string_lossy().into_owned()),
                    runtime_pid: self.launch.runtime.pid,
                    runtime_container_name,
                };
                info!(
                    user_id = %self.user_id,
                    healthy = status.healthy,
                    shutdown = status.shutdown,
                    sidecar_available = status.startup_status.sidecar_available,
                    shell_available = status.shell_available,
                    browser_available = status.browser_available,
                    degraded_reasons = ?status.startup_status.degraded_reasons,
                    "runner control health check handled"
                );
                RunnerControlResponse::HealthStatus(status)
            }
            RunnerControl::ShutdownUser { user_id } => {
                if user_id != self.user_id {
                    warn!(
                        requested_user = %user_id,
                        active_user = %self.user_id,
                        "runner control shutdown rejected for unknown user"
                    );
                    return RunnerControlResponse::Error(RunnerControlError {
                        code: RunnerControlErrorCode::UnknownUser,
                        message: format!(
                            "shutdown request targeted unknown user `{user_id}`; active user is `{}`",
                            self.user_id
                        ),
                    });
                }

                if self.shutdown_complete {
                    info!(user_id = %self.user_id, "runner control shutdown acknowledged as already stopped");
                    return RunnerControlResponse::ShutdownStatus(RunnerControlShutdownStatus {
                        user_id,
                        shutdown: true,
                        already_stopped: true,
                        message: Some("user runtime already shut down".to_owned()),
                    });
                }

                match self.shutdown() {
                    Ok(()) => {
                        info!(user_id = %self.user_id, "runner control shutdown completed");
                        RunnerControlResponse::ShutdownStatus(RunnerControlShutdownStatus {
                            user_id,
                            shutdown: true,
                            already_stopped: false,
                            message: Some("shutdown completed".to_owned()),
                        })
                    }
                    Err(error) => {
                        warn!(
                            user_id = %self.user_id,
                            error = %error,
                            "runner control shutdown failed"
                        );
                        RunnerControlResponse::Error(RunnerControlError {
                            code: RunnerControlErrorCode::Internal,
                            message: format!(
                                "failed to shut down user `{}`: {error}",
                                self.user_id
                            ),
                        })
                    }
                }
            }
        }
    }

    /// Async variant of [`Self::handle_control`] for use inside the daemon
    /// control loop. Uses async shutdown to avoid nested tokio runtimes.
    pub async fn handle_control_async(
        &mut self,
        request: RunnerControl,
    ) -> RunnerControlResponse {
        match request {
            RunnerControl::HealthCheck => self.handle_control(request),
            RunnerControl::ShutdownUser { user_id } => {
                if user_id != self.user_id {
                    warn!(
                        requested_user = %user_id,
                        active_user = %self.user_id,
                        "runner control shutdown rejected for unknown user"
                    );
                    return RunnerControlResponse::Error(RunnerControlError {
                        code: RunnerControlErrorCode::UnknownUser,
                        message: format!(
                            "shutdown request targeted unknown user `{user_id}`; active user is `{}`",
                            self.user_id
                        ),
                    });
                }

                if self.shutdown_complete {
                    info!(user_id = %self.user_id, "runner control shutdown acknowledged as already stopped");
                    return RunnerControlResponse::ShutdownStatus(
                        RunnerControlShutdownStatus {
                            user_id,
                            shutdown: true,
                            already_stopped: true,
                            message: Some("user runtime already shut down".to_owned()),
                        },
                    );
                }

                match self.shutdown_async().await {
                    Ok(()) => {
                        info!(user_id = %self.user_id, "runner control shutdown completed");
                        RunnerControlResponse::ShutdownStatus(
                            RunnerControlShutdownStatus {
                                user_id,
                                shutdown: true,
                                already_stopped: false,
                                message: Some("shutdown completed".to_owned()),
                            },
                        )
                    }
                    Err(error) => {
                        warn!(
                            user_id = %self.user_id,
                            error = %error,
                            "runner control shutdown failed"
                        );
                        RunnerControlResponse::Error(RunnerControlError {
                            code: RunnerControlErrorCode::Internal,
                            message: format!(
                                "failed to shut down user `{}`: {error}",
                                self.user_id
                            ),
                        })
                    }
                }
            }
        }
    }

    pub async fn serve_control_stream<Stream>(
        &mut self,
        mut stream: Stream,
    ) -> Result<(), RunnerControlTransportError>
    where
        Stream: AsyncRead + AsyncWrite + Unpin,
    {
        while let Some(frame) = read_runner_control_frame(&mut stream).await? {
            let response = match serde_json::from_slice::<RunnerControl>(&frame) {
                Ok(request) => self.handle_control_async(request).await,
                Err(error) => RunnerControlResponse::Error(RunnerControlError {
                    code: RunnerControlErrorCode::InvalidRequest,
                    message: format!("invalid runner control request frame: {error}"),
                }),
            };
            let payload = serde_json::to_vec(&response)?;
            write_runner_control_frame(&mut stream, &payload).await?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub async fn serve_control_unix_listener(
        &mut self,
        listener: tokio::net::UnixListener,
    ) -> Result<(), RunnerControlTransportError> {
        loop {
            let (stream, _) = listener.accept().await?;
            self.serve_control_stream(stream).await?;
            if self.shutdown_complete {
                return Ok(());
            }
        }
    }

    fn control_health_message(&self) -> Option<String> {
        if self.shutdown_complete {
            return Some("runtime is shut down".to_owned());
        }
        if self.warnings.is_empty() {
            None
        } else {
            Some(self.warnings.join(" | "))
        }
    }

    fn control_startup_status(&self) -> StartupStatusReport {
        let mut status = self.startup_status.clone();
        status.shell_available = self.shell_available;
        status.browser_available = self.browser_available;
        status.sidecar_available = status.shell_available || status.browser_available;
        if self.shutdown_complete {
            status.push_reason(
                StartupDegradedReasonCode::RuntimeShutdown,
                "runtime is shut down",
            );
        }
        status
    }
}

#[derive(Debug, Error)]
pub enum RunnerControlTransportError {
    #[error("runner control transport failure: {0}")]
    Transport(#[from] io::Error),
    #[error("runner control serialization failure: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("runner control frame too large: {actual_bytes} bytes exceeds max {max_bytes} bytes")]
    FrameTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

impl RunnerControlTransportError {
    /// Returns `true` when the underlying I/O error is `ConnectionRefused`,
    /// which typically means no daemon is listening on the control socket.
    pub fn is_connection_refused(&self) -> bool {
        matches!(
            self,
            Self::Transport(error) if error.kind() == io::ErrorKind::ConnectionRefused
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMountPaths {
    pub shared: PathBuf,
    pub tmp: PathBuf,
    pub vault: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLaunchSettings {
    pub mounts: EffectiveMountPaths,
    pub resources: RunnerResourceLimits,
    pub credential_refs: BTreeMap<String, String>,
}

impl EffectiveLaunchSettings {
    fn runtime_policy(&self) -> RunnerRuntimePolicy {
        RunnerRuntimePolicy {
            mounts: RunnerResolvedMountPaths {
                shared: self.mounts.shared.to_string_lossy().into_owned(),
                tmp: self.mounts.tmp.to_string_lossy().into_owned(),
                vault: self.mounts.vault.to_string_lossy().into_owned(),
            },
            resources: self.resources.clone(),
            credential_refs: self.credential_refs.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLaunchRequest {
    pub user_id: String,
    pub host_os: String,
    pub sandbox_tier: SandboxTier,
    pub workspace: UserWorkspace,
    pub mounts: EffectiveMountPaths,
    pub resource_limits: RunnerResourceLimits,
    pub credential_refs: BTreeMap<String, String>,
    pub guest_images: RunnerGuestImages,
    pub requested_shell: bool,
    pub requested_browser: bool,
    /// Path to the bootstrap envelope JSON file, pre-written for Container/MicroVM tiers.
    pub bootstrap_file: Option<PathBuf>,
    /// Extra environment variables to inject into the oxydra-vm container (`KEY=VALUE`).
    /// Contains config-referenced API keys and non-`SHELL_`-prefixed CLI env vars.
    pub extra_env: Vec<String>,
    /// Extra environment variables to inject into the shell-vm container (`KEY=VALUE`).
    /// Contains `[tools.shell.env_keys]`-resolved vars and `SHELL_`-prefixed CLI
    /// env vars (with the prefix stripped).
    pub shell_env: Vec<String>,
}

impl SandboxLaunchRequest {
    fn sidecar_requested(&self) -> bool {
        self.requested_shell || self.requested_browser
    }
}

#[derive(Debug)]
pub struct SandboxLaunch {
    pub launch: RunnerLaunchHandle,
    pub sidecar_endpoint: Option<SidecarEndpoint>,
    pub shell_available: bool,
    pub browser_available: bool,
    pub degraded_reasons: Vec<StartupDegradedReason>,
    pub warnings: Vec<String>,
}

pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError>;
}

#[derive(Debug, Default)]
pub struct CrateSandboxBackend;

pub type CommandSandboxBackend = CrateSandboxBackend;

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error("failed to read runner config `{path}`: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to parse runner global config `{path}`: {source}")]
    ParseGlobalConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to parse runner user config `{path}`: {source}")]
    ParseUserConfig {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error(transparent)]
    ConfigValidation(#[from] RunnerConfigError),
    #[error(transparent)]
    Bootstrap(#[from] BootstrapEnvelopeError),
    #[error("unknown runner user `{user_id}`")]
    UnknownUser { user_id: String },
    #[error("invalid runner user id `{user_id}`")]
    InvalidUserId { user_id: String },
    #[error("microvm tier is unsupported on host `{os}`")]
    UnsupportedMicroVmHost { os: String },
    #[error("linux microvm requires `{field}` in [guest_images] config")]
    MissingFirecrackerConfig { field: &'static str },
    #[error("failed to parse firecracker config `{path}`: {source}")]
    ParseFirecrackerConfig {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "bootstrap payload too large for kernel cmdline: {bytes} bytes exceeds max {max_bytes} bytes"
    )]
    BootstrapTooLargeForCmdline { bytes: usize, max_bytes: usize },
    #[error("failed to launch `{role}` guest with `{program}`: {source}")]
    LaunchGuest {
        role: RunnerGuestRole,
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to {action} `{role}` guest `{program}`: {source}")]
    GuestLifecycle {
        action: &'static str,
        role: RunnerGuestRole,
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("failed to initialize async runtime: {source}")]
    AsyncRuntimeInit {
        #[source]
        source: io::Error,
    },
    #[error("docker connection to `{endpoint}` failed: {message}")]
    DockerConnect { endpoint: String, message: String },
    #[error("docker operation `{operation}` for `{target}` on `{endpoint}` failed: {message}")]
    DockerOperation {
        endpoint: String,
        operation: &'static str,
        target: String,
        message: String,
    },
    #[error("failed to resolve HOME for Docker sandbox socket path")]
    MissingHomeDirectory,
    #[error("sandbox VM `{vm_name}` did not return a docker socket path")]
    SandboxVmMissingDockerSocket { vm_name: String },
    #[error("sandbox API `{operation}` request build failed: {message}")]
    SandboxApiRequestBuild {
        operation: &'static str,
        message: String,
    },
    #[error("sandbox API `{operation}` transport failure on `{socket_path}`: {message}")]
    SandboxApiTransport {
        operation: &'static str,
        socket_path: PathBuf,
        message: String,
    },
    #[error("sandbox API `{operation}` returned {status} with body `{body}`")]
    SandboxApiStatus {
        operation: &'static str,
        status: u16,
        body: String,
    },
    #[error(
        "firecracker API socket `{path}` did not become ready within {timeout_secs}s (last error: {last_error})"
    )]
    FirecrackerApiNotReady {
        path: PathBuf,
        timeout_secs: u64,
        last_error: String,
    },
    #[error("failed to provision workspace directory `{path}`: {source}")]
    ProvisionWorkspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "no running guest found for user `{user_id}`; expected gateway endpoint marker at `{endpoint_path}`"
    )]
    NoRunningGuest {
        user_id: String,
        endpoint_path: PathBuf,
    },
    #[error("failed to read gateway endpoint marker `{path}`: {source}")]
    ReadGatewayEndpoint {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("gateway endpoint marker `{path}` is empty")]
    InvalidGatewayEndpoint { path: PathBuf },
    #[error("failed to probe gateway endpoint `{endpoint}`: {message}")]
    GatewayProbeFailed { endpoint: String, message: String },
    #[error(
        "found gateway endpoint marker at `{marker_path}` but the gateway is not responding \
         (`{endpoint}`): {message}\n\
         The previous session may have exited without cleanup. \
         Remove `{marker_path}` and restart."
    )]
    StaleGatewayEndpoint {
        endpoint: String,
        marker_path: PathBuf,
        message: String,
    },
    #[error("failed to transfer image `{image}` into sandbox VM: {message}")]
    ImageTransfer { image: String, message: String },
}

#[derive(Debug, Clone, Copy)]
struct RequestedCapabilities {
    shell: bool,
    browser: bool,
}

fn build_startup_status_report(
    sandbox_tier: SandboxTier,
    capabilities: RequestedCapabilities,
    shell_available: bool,
    browser_available: bool,
    sidecar_available: bool,
    degraded_reasons: &[StartupDegradedReason],
) -> StartupStatusReport {
    let mut startup_status = StartupStatusReport {
        sandbox_tier,
        sidecar_available,
        shell_available,
        browser_available,
        degraded_reasons: degraded_reasons.to_vec(),
    };

    if sandbox_tier == SandboxTier::Process
        && !startup_status.has_reason_code(StartupDegradedReasonCode::InsecureProcessTier)
    {
        startup_status.push_reason(
            StartupDegradedReasonCode::InsecureProcessTier,
            PROCESS_TIER_WARNING,
        );
    }

    if (capabilities.shell || capabilities.browser) && !sidecar_available {
        startup_status.push_reason(
            StartupDegradedReasonCode::SidecarUnavailable,
            "shell/browser capabilities were requested but no sidecar endpoint is available",
        );
    }

    startup_status
}

fn resolve_requested_capabilities(
    sandbox_tier: SandboxTier,
    user_config: &RunnerUserConfig,
) -> RequestedCapabilities {
    let mut shell = !matches!(sandbox_tier, SandboxTier::Process);
    let mut browser = !matches!(sandbox_tier, SandboxTier::Process);

    if let Some(enabled) = user_config.behavior.shell_enabled {
        shell &= enabled;
    }
    if let Some(enabled) = user_config.behavior.browser_enabled {
        browser &= enabled;
    }

    RequestedCapabilities { shell, browser }
}

/// Pre-computes the sidecar endpoint address so the bootstrap envelope can be
/// written to disk before launching the backend. The addresses are deterministic
/// based on tier, host OS, and workspace layout.
fn pre_compute_sidecar_endpoint(
    sandbox_tier: SandboxTier,
    host_os: &str,
    workspace: &UserWorkspace,
) -> SidecarEndpoint {
    match (sandbox_tier, host_os) {
        (SandboxTier::MicroVm, "linux") => SidecarEndpoint {
            transport: SidecarTransport::Vsock,
            address: format!(
                "unix://{}",
                workspace
                    .ipc
                    .join("shell-daemon-vsock.sock")
                    .to_string_lossy()
            ),
        },
        _ => SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: workspace
                .ipc
                .join("shell-daemon.sock")
                .to_string_lossy()
                .into_owned(),
        },
    }
}

fn resolve_effective_launch_settings(
    workspace: &UserWorkspace,
    user_config: &RunnerUserConfig,
) -> Result<EffectiveLaunchSettings, RunnerError> {
    let mounts = resolve_effective_mount_paths(workspace, &user_config.mounts);
    for path in [&mounts.shared, &mounts.tmp, &mounts.vault] {
        fs::create_dir_all(path).map_err(|source| RunnerError::ProvisionWorkspace {
            path: path.clone(),
            source,
        })?;
    }

    Ok(EffectiveLaunchSettings {
        mounts,
        resources: user_config.resources.clone(),
        credential_refs: user_config.credential_refs.clone(),
    })
}

fn resolve_effective_mount_paths(
    workspace: &UserWorkspace,
    configured: &RunnerMountPaths,
) -> EffectiveMountPaths {
    EffectiveMountPaths {
        shared: resolve_mount_path_override(
            workspace,
            configured.shared.as_deref(),
            &workspace.shared,
        ),
        tmp: resolve_mount_path_override(workspace, configured.tmp.as_deref(), &workspace.tmp),
        vault: resolve_mount_path_override(
            workspace,
            configured.vault.as_deref(),
            &workspace.vault,
        ),
    }
}

fn resolve_mount_path_override(
    workspace: &UserWorkspace,
    configured: Option<&str>,
    default: &Path,
) -> PathBuf {
    let Some(configured) = configured.map(str::trim).filter(|value| !value.is_empty()) else {
        return default.to_path_buf();
    };
    let configured = PathBuf::from(configured);
    if configured.is_absolute() {
        configured
    } else {
        workspace.root.join(configured)
    }
}

fn docker_guest_container_name(tier_label: &str, user_id: &str, role: RunnerGuestRole) -> String {
    let user_component = sanitize_container_component(user_id);
    format!("oxydra-{tier_label}-{user_component}-{}", role.as_label())
}

fn docker_guest_command_spec(endpoint: &DockerEndpoint, container_name: &str) -> RunnerCommandSpec {
    RunnerCommandSpec::new(
        "bollard",
        vec![
            "--endpoint".to_owned(),
            endpoint.label(),
            "--container".to_owned(),
            container_name.to_owned(),
        ],
    )
}

fn docker_guest_labels(
    tier_label: &str,
    user_id: &str,
    role: RunnerGuestRole,
) -> HashMap<String, String> {
    HashMap::from([
        ("oxydra.sandbox_tier".to_owned(), tier_label.to_owned()),
        ("oxydra.user_id".to_owned(), user_id.to_owned()),
        ("oxydra.guest_role".to_owned(), role.as_label().to_owned()),
    ])
}

struct DockerContainerLaunchParams {
    endpoint: DockerEndpoint,
    container_name: String,
    image: String,
    role: RunnerGuestRole,
    user_id: String,
    workspace: UserWorkspace,
    mounts: EffectiveMountPaths,
    resource_limits: RunnerResourceLimits,
    labels: HashMap<String, String>,
    bootstrap_file: Option<PathBuf>,
    /// Extra environment variables to inject into the container (`KEY=VALUE`).
    /// For OxydraVm this includes API keys; for ShellVm only shell-specific vars.
    extra_env: Vec<String>,
}

async fn launch_docker_container_async(
    params: DockerContainerLaunchParams,
) -> Result<(), RunnerError> {
    let docker = docker_client(&params.endpoint)?;
    remove_container_if_exists(&docker, &params.endpoint, &params.container_name).await?;

    // Pull the image if it doesn't exist locally.
    if docker.inspect_image(&params.image).await.is_err() {
        let (from_image, tag) = match params.image.rsplit_once(':') {
            Some((img, tag)) => (img, tag),
            None => (params.image.as_str(), "latest"),
        };
        docker
            .create_image(
                Some(
                    CreateImageOptionsBuilder::new()
                        .from_image(from_image)
                        .tag(tag)
                        .build(),
                ),
                None,
                None,
            )
            .try_collect::<Vec<_>>()
            .await
            .map_err(|source| {
                docker_operation_error(
                    &params.endpoint,
                    "pull_image",
                    &params.container_name,
                    source,
                )
            })?;
    }

    // Compute entrypoint/cmd before binds consumes workspace paths.
    let (entrypoint, cmd) = match params.role {
        RunnerGuestRole::OxydraVm => {
            let mut cmd_args = vec![
                "--user-id".to_owned(),
                params.user_id,
                "--workspace-root".to_owned(),
                params.workspace.root.to_string_lossy().into_owned(),
            ];
            if params.bootstrap_file.is_some() {
                cmd_args.push("--bootstrap-file".to_owned());
                cmd_args.push(CONTAINER_BOOTSTRAP_MOUNT_PATH.to_owned());
            }
            (vec!["/usr/local/bin/oxydra-vm".to_owned()], cmd_args)
        }
        RunnerGuestRole::ShellVm => {
            // ShellVm uses virtual container paths (e.g., /shared, /tmp),
            // so the socket path is relative to the container namespace.
            (
                vec!["/usr/local/bin/shell-daemon".to_owned()],
                vec!["--socket".to_owned(), "/ipc/shell-daemon.sock".to_owned()],
            )
        }
    };

    // OxydraVm: working_dir is the host workspace root so that
    // ConfigSearchPaths::discover() can resolve CWD/.oxydra/.
    // ShellVm: working_dir is /shared (container-virtual path).
    let working_dir = match params.role {
        RunnerGuestRole::OxydraVm => Some(params.workspace.root.to_string_lossy().into_owned()),
        RunnerGuestRole::ShellVm => Some("/shared".to_owned()),
    };

    let mut binds: Vec<String> = match params.role {
        RunnerGuestRole::OxydraVm => {
            // OxydraVm gets the full workspace root (includes .oxydra/ internal dir)
            [
                params.workspace.root,
                params.mounts.shared,
                params.mounts.tmp,
                params.mounts.vault,
            ]
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|path| format!("{path}:{path}"))
            .collect()
        }
        RunnerGuestRole::ShellVm => {
            // ShellVm uses virtual container paths so that shell output
            // does not leak host filesystem details to the LLM.
            vec![
                format!("{}:/shared", params.mounts.shared.to_string_lossy()),
                format!("{}:/tmp", params.mounts.tmp.to_string_lossy()),
                format!("{}:/ipc", params.workspace.ipc.to_string_lossy()),
            ]
        }
    };

    let mut env = Vec::new();
    if matches!(params.endpoint, DockerEndpoint::UnixSocket(_)) {
        env.push("HTTP_PROXY=http://host.docker.internal:3128".to_owned());
        env.push("HTTPS_PROXY=http://host.docker.internal:3128".to_owned());
        env.push("http_proxy=http://host.docker.internal:3128".to_owned());
        env.push("https_proxy=http://host.docker.internal:3128".to_owned());
    }
    if let Some(ref bootstrap_path) = params.bootstrap_file {
        binds.push(format!(
            "{}:{}:ro",
            bootstrap_path.to_string_lossy(),
            CONTAINER_BOOTSTRAP_MOUNT_PATH
        ));
        env.push(format!(
            "{CONTAINER_BOOTSTRAP_ENV_KEY}={CONTAINER_BOOTSTRAP_MOUNT_PATH}"
        ));
    }
    env.extend(params.extra_env);

    let config = ContainerCreateBody {
        image: Some(params.image),
        labels: Some(params.labels),
        entrypoint: Some(entrypoint),
        cmd: Some(cmd),
        working_dir,
        env: if env.is_empty() { None } else { Some(env) },
        host_config: Some(HostConfig {
            binds: Some(binds),
            network_mode: Some("host".to_owned()),
            nano_cpus: params
                .resource_limits
                .max_vcpus
                .map(|max_vcpus| i64::from(max_vcpus) * 1_000_000_000),
            memory: params
                .resource_limits
                .max_memory_mib
                .map(|max_memory_mib| (max_memory_mib as i64) * 1024 * 1024),
            pids_limit: params.resource_limits.max_processes.map(i64::from),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::ON_FAILURE),
                maximum_retry_count: Some(5),
            }),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::new()
                    .name(&params.container_name)
                    .build(),
            ),
            config,
        )
        .await
        .map_err(|source| {
            docker_operation_error(
                &params.endpoint,
                "create_container",
                &params.container_name,
                source,
            )
        })?;

    docker
        .start_container(
            &params.container_name,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .map_err(|source| {
            docker_operation_error(
                &params.endpoint,
                "start_container",
                &params.container_name,
                source,
            )
        })?;
    Ok(())
}

fn shutdown_docker_container(handle: DockerContainerHandle) -> Result<(), RunnerError> {
    run_async(async move {
        let docker = docker_client(&handle.endpoint)?;
        let _ = docker
            .stop_container(
                &handle.container_name,
                Some(StopContainerOptionsBuilder::new().t(5).build()),
            )
            .await;
        remove_container_if_exists(&docker, &handle.endpoint, &handle.container_name).await
    })
}

async fn shutdown_docker_container_async(
    handle: DockerContainerHandle,
) -> Result<(), RunnerError> {
    let docker = docker_client(&handle.endpoint)?;
    let _ = docker
        .stop_container(
            &handle.container_name,
            Some(StopContainerOptionsBuilder::new().t(5).build()),
        )
        .await;
    remove_container_if_exists(&docker, &handle.endpoint, &handle.container_name).await
}

async fn remove_container_if_exists(
    docker: &Docker,
    endpoint: &DockerEndpoint,
    container_name: &str,
) -> Result<(), RunnerError> {
    match docker
        .remove_container(
            container_name,
            Some(
                RemoveContainerOptionsBuilder::new()
                    .force(true)
                    .v(true)
                    .link(false)
                    .build(),
            ),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if is_not_found_error(&error) => Ok(()),
        Err(error) => Err(docker_operation_error(
            endpoint,
            "remove_container",
            container_name,
            error,
        )),
    }
}

fn is_not_found_error(error: &BollardError) -> bool {
    match error {
        BollardError::DockerResponseServerError { status_code, .. } => *status_code == 404,
        _ => false,
    }
}

fn docker_client(endpoint: &DockerEndpoint) -> Result<Docker, RunnerError> {
    match endpoint {
        DockerEndpoint::Local => {
            Docker::connect_with_local_defaults().map_err(|source| RunnerError::DockerConnect {
                endpoint: endpoint.label(),
                message: source.to_string(),
            })
        }
        DockerEndpoint::UnixSocket(path) => {
            Docker::connect_with_socket(path, DEFAULT_DOCKER_TIMEOUT_SECS, API_DEFAULT_VERSION)
                .map_err(|source| RunnerError::DockerConnect {
                    endpoint: endpoint.label(),
                    message: source.to_string(),
                })
        }
    }
}

fn docker_operation_error(
    endpoint: &DockerEndpoint,
    operation: &'static str,
    target: &str,
    source: BollardError,
) -> RunnerError {
    RunnerError::DockerOperation {
        endpoint: endpoint.label(),
        operation,
        target: target.to_owned(),
        message: source.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DockerSandboxVmInfo {
    vm_name: String,
    docker_socket_path: PathBuf,
}

#[derive(Debug, Clone)]
struct UnixJsonResponse {
    status: StatusCode,
    body: Value,
    raw_body: String,
}

fn docker_sandboxd_socket_path() -> Result<PathBuf, RunnerError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(RunnerError::MissingHomeDirectory)?;
    Ok(home.join(DOCKER_SANDBOXD_SOCKET_RELATIVE_PATH))
}

fn create_docker_sandbox_vm_sync(
    sandbox_socket_path: PathBuf,
    agent_name: String,
    workspace_dir: PathBuf,
) -> Result<DockerSandboxVmInfo, RunnerError> {
    run_async(async move {
        let vm_resource_name = docker_sandbox_vm_resource_name(&agent_name);
        let _ =
            delete_docker_sandbox_vm_async(sandbox_socket_path.clone(), vm_resource_name.clone())
                .await;
        let payload = json!({
            "agent_name": agent_name,
            "workspace_dir": workspace_dir.to_string_lossy(),
        });
        let response = send_unix_json_request(
            &sandbox_socket_path,
            Method::POST,
            DOCKER_SANDBOX_VM_ENDPOINT,
            Some(payload),
            "create_sandbox_vm",
        )
        .await?;
        if !response.status.is_success() {
            return Err(RunnerError::SandboxApiStatus {
                operation: "create_sandbox_vm",
                status: response.status.as_u16(),
                body: response.raw_body,
            });
        }

        let docker_socket_path = extract_socket_path(&response.body).ok_or(
            RunnerError::SandboxVmMissingDockerSocket {
                vm_name: vm_resource_name.clone(),
            },
        )?;

        Ok(DockerSandboxVmInfo {
            vm_name: vm_resource_name,
            docker_socket_path,
        })
    })
}

fn docker_sandbox_vm_resource_name(agent_name: &str) -> String {
    format!("{agent_name}-vm")
}

fn load_image_into_sandbox_vm(image: &str, vm_socket_path: &str) -> Result<(), RunnerError> {
    let mut save_child = Command::new("docker")
        .args(["save", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: format!("failed to start 'docker save': {source}"),
        })?;

    let save_stdout = save_child
        .stdout
        .take()
        .ok_or_else(|| RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: "failed to capture 'docker save' stdout".to_owned(),
        })?;

    let load_output = Command::new("docker")
        .args(["--host", &format!("unix://{vm_socket_path}"), "load"])
        .stdin(save_stdout)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: format!("failed to run 'docker load': {source}"),
        })?;

    let save_status = save_child
        .wait()
        .map_err(|source| RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: format!("failed to wait for 'docker save': {source}"),
        })?;

    if !save_status.success() {
        return Err(RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: format!("'docker save' exited with status {save_status}"),
        });
    }
    if !load_output.status.success() {
        let stderr = String::from_utf8_lossy(&load_output.stderr);
        return Err(RunnerError::ImageTransfer {
            image: image.to_owned(),
            message: format!(
                "'docker load' exited with status {}: {stderr}",
                load_output.status
            ),
        });
    }
    info!(image = %image, vm_socket = %vm_socket_path, "loaded image into sandbox VM");
    Ok(())
}

fn delete_docker_sandbox_vm_sync(
    sandbox_socket_path: PathBuf,
    vm_name: String,
) -> Result<(), RunnerError> {
    run_async(delete_docker_sandbox_vm_async(sandbox_socket_path, vm_name))
}

async fn delete_docker_sandbox_vm_async(
    sandbox_socket_path: PathBuf,
    vm_name: String,
) -> Result<(), RunnerError> {
    let response = send_unix_json_request(
        &sandbox_socket_path,
        Method::DELETE,
        &format!("{DOCKER_SANDBOX_VM_ENDPOINT}/{vm_name}"),
        None,
        "delete_sandbox_vm",
    )
    .await?;
    if response.status == StatusCode::NOT_FOUND || response.status.is_success() {
        Ok(())
    } else {
        Err(RunnerError::SandboxApiStatus {
            operation: "delete_sandbox_vm",
            status: response.status.as_u16(),
            body: response.raw_body,
        })
    }
}

async fn send_unix_json_request(
    socket_path: &Path,
    method: Method,
    path: &str,
    body: Option<Value>,
    operation: &'static str,
) -> Result<UnixJsonResponse, RunnerError> {
    let client: Client<UnixConnector, Full<Bytes>> = Client::unix();
    let uri: Uri = HyperlocalUri::new(socket_path, path).into();

    let mut request_builder = Request::builder().method(method).uri(uri);
    let request_body = match body {
        Some(payload) => {
            request_builder = request_builder.header("content-type", "application/json");
            Full::new(Bytes::from(payload.to_string()))
        }
        None => Full::new(Bytes::new()),
    };

    let request = request_builder.body(request_body).map_err(|error| {
        RunnerError::SandboxApiRequestBuild {
            operation,
            message: error.to_string(),
        }
    })?;

    let response =
        client
            .request(request)
            .await
            .map_err(|error| RunnerError::SandboxApiTransport {
                operation,
                socket_path: socket_path.to_path_buf(),
                message: error.to_string(),
            })?;
    let status = response.status();
    let body_bytes = response
        .into_body()
        .collect()
        .await
        .map_err(|error| RunnerError::SandboxApiTransport {
            operation,
            socket_path: socket_path.to_path_buf(),
            message: error.to_string(),
        })?
        .to_bytes();
    let raw_body = String::from_utf8_lossy(&body_bytes).into_owned();
    let body = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or(Value::String(raw_body.clone()))
    };
    Ok(UnixJsonResponse {
        status,
        body,
        raw_body,
    })
}

fn extract_socket_path(payload: &Value) -> Option<PathBuf> {
    match payload {
        Value::String(value) => value
            .trim()
            .strip_prefix("unix://")
            .map(PathBuf::from)
            .or_else(|| {
                if value.trim().ends_with(".sock") {
                    Some(PathBuf::from(value.trim()))
                } else {
                    None
                }
            }),
        Value::Object(object) => {
            for key in [
                "socketPath",
                "socket_path",
                "dockerSocketPath",
                "docker_socket_path",
            ] {
                if let Some(path) = object.get(key).and_then(Value::as_str) {
                    let trimmed = path.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Some(stripped) = trimmed.strip_prefix("unix://") {
                        return Some(PathBuf::from(stripped));
                    }
                    if trimmed.ends_with(".sock") {
                        return Some(PathBuf::from(trimmed));
                    }
                }
            }

            for value in object.values() {
                if let Some(path) = extract_socket_path(value) {
                    return Some(path);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(extract_socket_path),
        _ => None,
    }
}

/// Spawns background threads that read from child stdout/stderr pipes and write
/// to log files. Returns `JoinHandle`s for cleanup on shutdown.
fn spawn_log_pump_threads(
    child: &mut Child,
    log_dir: &Path,
    role: RunnerGuestRole,
) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::new();

    if let Some(stdout) = child.stdout.take() {
        let path = log_dir.join(format!("{}.stdout.log", role.as_label()));
        handles.push(std::thread::spawn(move || pump_pipe_to_file(stdout, &path)));
    }
    if let Some(stderr) = child.stderr.take() {
        let path = log_dir.join(format!("{}.stderr.log", role.as_label()));
        handles.push(std::thread::spawn(move || pump_pipe_to_file(stderr, &path)));
    }

    handles
}

fn pump_pipe_to_file(pipe: impl std::io::Read, path: &Path) {
    use std::io::BufRead;

    let reader = std::io::BufReader::new(pipe);
    let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    for line in reader.lines() {
        let Ok(line) = line else {
            break;
        };
        let _ = writeln!(file, "{line}");
    }
}

const CONTAINER_BOOTSTRAP_MOUNT_PATH: &str = "/run/oxydra/bootstrap";
const CONTAINER_BOOTSTRAP_ENV_KEY: &str = "OXYDRA_BOOTSTRAP_FILE";

fn write_bootstrap_file(
    workspace: &UserWorkspace,
    bootstrap: &RunnerBootstrapEnvelope,
) -> Result<PathBuf, RunnerError> {
    let bootstrap_dir = workspace.ipc.join("bootstrap");
    fs::create_dir_all(&bootstrap_dir).map_err(|source| RunnerError::ProvisionWorkspace {
        path: bootstrap_dir.clone(),
        source,
    })?;
    let bootstrap_path = bootstrap_dir.join("runner_bootstrap.json");
    let payload = serde_json::to_vec_pretty(bootstrap).map_err(BootstrapEnvelopeError::from)?;
    if fs::exists(&bootstrap_path).unwrap_or(false) {
        fs::remove_file(&bootstrap_path).map_err(|source| RunnerError::ProvisionWorkspace {
            path: bootstrap_path.clone(),
            source,
        })?;
    }
    fs::write(&bootstrap_path, &payload).map_err(|source| RunnerError::ProvisionWorkspace {
        path: bootstrap_path.clone(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&bootstrap_path, fs::Permissions::from_mode(0o400));
    }
    Ok(bootstrap_path)
}

/// Copy the host's agent configuration files (`agent.toml` and `providers.toml`)
/// into the user's workspace internal directory (`<workspace>/.oxydra/`).
/// This makes the config available inside the container, which mounts the
/// workspace root. Files are copied (not moved) so the host config is preserved.
fn copy_agent_config_to_workspace(workspace: &UserWorkspace) -> Result<(), RunnerError> {
    let host_paths = bootstrap::ConfigSearchPaths::discover().map_err(|source| {
        RunnerError::ProvisionWorkspace {
            path: workspace.internal.clone(),
            source: io::Error::other(source.to_string()),
        }
    })?;

    for file_name in [
        bootstrap::AGENT_CONFIG_FILE_NAME,
        bootstrap::PROVIDERS_CONFIG_FILE_NAME,
    ] {
        let source_path = host_paths.workspace_dir.join(file_name);
        if source_path.is_file() {
            let dest_path = workspace.internal.join(file_name);
            fs::copy(&source_path, &dest_path).map_err(|source| {
                RunnerError::ProvisionWorkspace {
                    path: dest_path,
                    source,
                }
            })?;
        }
    }

    Ok(())
}

/// Collect environment variable names that the agent config references for API
/// keys and other credentials. For each `api_key_env` field in the provider
/// registry and web-search config, plus well-known provider-type defaults
/// (e.g. `OPENAI_API_KEY`), look up the value in the runner's own environment
/// and return `KEY=VALUE` pairs for variables that are set.
fn collect_config_env_vars() -> Vec<String> {
    let agent_config = match load_agent_config(None, CliOverrides::default()) {
        Ok(config) => config,
        Err(_) => return Vec::new(),
    };

    let mut env_var_names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Collect api_key_env from all provider registry entries.
    for entry in agent_config.providers.registry.values() {
        if let Some(ref key_env) = entry.api_key_env
            && !key_env.is_empty()
        {
            env_var_names.insert(key_env.clone());
        }
    }

    // Collect web search config env var references.
    if let Some(ref ws) = agent_config.tools.web_search {
        if let Some(ref key_env) = ws.api_key_env
            && !key_env.is_empty()
        {
            env_var_names.insert(key_env.clone());
        }
        if let Some(ref engine_env) = ws.engine_id_env
            && !engine_env.is_empty()
        {
            env_var_names.insert(engine_env.clone());
        }
    }

    // Resolve each env var name against the runner's own environment.
    let mut result = Vec::new();
    for var_name in &env_var_names {
        if let Ok(value) = std::env::var(var_name)
            && !value.is_empty()
        {
            result.push(format!("{var_name}={value}"));
        }
    }
    result
}

/// Collect environment variables specified in `[tools.shell.env_keys]` from
/// the agent config. For each listed key name that is set in the runner's own
/// environment, returns a `KEY=VALUE` pair for injection into the shell-vm
/// container.
fn collect_shell_config_env_keys() -> Vec<String> {
    let agent_config = match load_agent_config(None, CliOverrides::default()) {
        Ok(config) => config,
        Err(_) => return Vec::new(),
    };

    let env_keys = match agent_config.tools.shell {
        Some(ref shell) => match shell.env_keys {
            Some(ref keys) => keys,
            None => return Vec::new(),
        },
        None => return Vec::new(),
    };

    let mut result = Vec::new();
    for var_name in env_keys {
        let var_name = var_name.trim();
        if var_name.is_empty() {
            continue;
        }
        if let Ok(value) = std::env::var(var_name)
            && !value.is_empty()
        {
            result.push(format!("{var_name}={value}"));
        }
    }
    result
}

fn ensure_firecracker_api_ready(api_socket_path: PathBuf) -> Result<(), RunnerError> {
    run_async(async move {
        let started = Instant::now();
        let mut last_error = String::from("firecracker API did not answer");
        while started.elapsed() < FIRECRACKER_API_READY_TIMEOUT {
            match send_unix_json_request(
                &api_socket_path,
                Method::GET,
                "/",
                None,
                "probe_firecracker_api",
            )
            .await
            {
                Ok(_) => return Ok(()),
                Err(error) => {
                    last_error = error.to_string();
                    tokio::time::sleep(FIRECRACKER_API_READY_POLL_INTERVAL).await;
                }
            }
        }

        Err(RunnerError::FirecrackerApiNotReady {
            path: api_socket_path,
            timeout_secs: FIRECRACKER_API_READY_TIMEOUT.as_secs(),
            last_error,
        })
    })
}

fn run_async<T, F>(future: F) -> Result<T, RunnerError>
where
    F: Future<Output = Result<T, RunnerError>>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|source| RunnerError::AsyncRuntimeInit { source })?;
    runtime.block_on(future)
}

async fn read_runner_control_frame<Stream>(
    stream: &mut Stream,
) -> Result<Option<Vec<u8>>, RunnerControlTransportError>
where
    Stream: AsyncRead + Unpin,
{
    let mut len_buffer = [0_u8; 4];
    match stream.read_exact(&mut len_buffer).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(RunnerControlTransportError::Transport(error)),
    }
    let frame_len = u32::from_be_bytes(len_buffer) as usize;
    if frame_len > RUNNER_CONTROL_MAX_FRAME_BYTES {
        return Err(RunnerControlTransportError::FrameTooLarge {
            actual_bytes: frame_len,
            max_bytes: RUNNER_CONTROL_MAX_FRAME_BYTES,
        });
    }
    let mut payload = vec![0_u8; frame_len];
    stream.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_runner_control_frame<Stream>(
    stream: &mut Stream,
    payload: &[u8],
) -> Result<(), RunnerControlTransportError>
where
    Stream: AsyncWrite + Unpin,
{
    if payload.len() > RUNNER_CONTROL_MAX_FRAME_BYTES {
        return Err(RunnerControlTransportError::FrameTooLarge {
            actual_bytes: payload.len(),
            max_bytes: RUNNER_CONTROL_MAX_FRAME_BYTES,
        });
    }
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| RunnerControlTransportError::FrameTooLarge {
            actual_bytes: payload.len(),
            max_bytes: RUNNER_CONTROL_MAX_FRAME_BYTES,
        })?;
    stream.write_all(&payload_len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

fn sanitize_container_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "user".to_owned()
    } else {
        sanitized
    }
}

fn read_gateway_endpoint_marker(path: &Path, user_id: &str) -> Result<String, RunnerError> {
    let raw = fs::read_to_string(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            RunnerError::NoRunningGuest {
                user_id: user_id.to_owned(),
                endpoint_path: path.to_path_buf(),
            }
        } else {
            RunnerError::ReadGatewayEndpoint {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    let endpoint = raw.trim();
    if endpoint.is_empty() {
        return Err(RunnerError::InvalidGatewayEndpoint {
            path: path.to_path_buf(),
        });
    }
    Ok(endpoint.to_owned())
}

fn probe_gateway_health(gateway_endpoint: &str, user_id: &str) -> Result<String, RunnerError> {
    let gateway_endpoint = gateway_endpoint.to_owned();
    let user_id = user_id.to_owned();
    run_async(async move {
        let (mut socket, _) = connect_async(&gateway_endpoint).await.map_err(|error| {
            RunnerError::GatewayProbeFailed {
                endpoint: gateway_endpoint.clone(),
                message: format!("websocket connect failed: {error}"),
            }
        })?;

        send_gateway_client_frame(
            &mut socket,
            &GatewayClientFrame::Hello(GatewayClientHello {
                request_id: "runner-connect-hello".to_owned(),
                protocol_version: GATEWAY_PROTOCOL_VERSION,
                user_id,
                session_id: None,
                create_new_session: false,
            }),
            &gateway_endpoint,
        )
        .await?;

        let session_id =
            match receive_gateway_server_frame(&mut socket, &gateway_endpoint).await? {
                GatewayServerFrame::HelloAck(ack) => ack.session.session_id,
                GatewayServerFrame::Error(error) => {
                    return Err(RunnerError::GatewayProbeFailed {
                        endpoint: gateway_endpoint,
                        message: format!("gateway rejected hello: {}", error.message),
                    });
                }
                frame => {
                    return Err(RunnerError::GatewayProbeFailed {
                        endpoint: gateway_endpoint,
                        message: format!("expected hello_ack, got {frame:?}"),
                    });
                }
            };

        send_gateway_client_frame(
            &mut socket,
            &GatewayClientFrame::HealthCheck(GatewayHealthCheck {
                request_id: "runner-connect-health".to_owned(),
            }),
            &gateway_endpoint,
        )
        .await?;

        loop {
            match receive_gateway_server_frame(&mut socket, &gateway_endpoint).await? {
                GatewayServerFrame::HealthStatus(status) if status.healthy => {
                    if let Some(startup_status) = status
                        .startup_status
                        .as_ref()
                        .filter(|value| value.is_degraded())
                    {
                        warn!(
                            endpoint = %gateway_endpoint,
                            sandbox_tier = ?startup_status.sandbox_tier,
                            sidecar_available = startup_status.sidecar_available,
                            shell_available = startup_status.shell_available,
                            browser_available = startup_status.browser_available,
                            degraded_reasons = ?startup_status.degraded_reasons,
                            "gateway reported degraded startup status during probe"
                        );
                    }
                    let _ = socket.close(None).await;
                    return Ok(session_id);
                }
                GatewayServerFrame::HealthStatus(status) => {
                    return Err(RunnerError::GatewayProbeFailed {
                        endpoint: gateway_endpoint,
                        message: status.message.unwrap_or_else(|| {
                            "gateway health check reported unhealthy".to_owned()
                        }),
                    });
                }
                GatewayServerFrame::Error(error) => {
                    return Err(RunnerError::GatewayProbeFailed {
                        endpoint: gateway_endpoint,
                        message: error.message,
                    });
                }
                _ => {}
            }
        }
    })
}

async fn send_gateway_client_frame(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    frame: &GatewayClientFrame,
    endpoint: &str,
) -> Result<(), RunnerError> {
    let payload =
        serde_json::to_string(frame).map_err(|error| RunnerError::GatewayProbeFailed {
            endpoint: endpoint.to_owned(),
            message: format!("gateway client frame serialization failed: {error}"),
        })?;
    socket
        .send(WsMessage::Text(payload.into()))
        .await
        .map_err(|error| RunnerError::GatewayProbeFailed {
            endpoint: endpoint.to_owned(),
            message: format!("gateway frame send failed: {error}"),
        })
}

async fn receive_gateway_server_frame(
    socket: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
    endpoint: &str,
) -> Result<GatewayServerFrame, RunnerError> {
    loop {
        let Some(message) = socket.next().await else {
            return Err(RunnerError::GatewayProbeFailed {
                endpoint: endpoint.to_owned(),
                message: "gateway websocket closed before probe completed".to_owned(),
            });
        };

        match message {
            Ok(WsMessage::Text(payload)) => {
                return serde_json::from_str::<GatewayServerFrame>(payload.as_ref()).map_err(
                    |error| RunnerError::GatewayProbeFailed {
                        endpoint: endpoint.to_owned(),
                        message: format!("gateway frame decode failed: {error}"),
                    },
                );
            }
            Ok(WsMessage::Binary(payload)) => {
                return serde_json::from_slice::<GatewayServerFrame>(&payload).map_err(|error| {
                    RunnerError::GatewayProbeFailed {
                        endpoint: endpoint.to_owned(),
                        message: format!("gateway frame decode failed: {error}"),
                    }
                });
            }
            Ok(WsMessage::Ping(payload)) => {
                socket
                    .send(WsMessage::Pong(payload))
                    .await
                    .map_err(|error| RunnerError::GatewayProbeFailed {
                        endpoint: endpoint.to_owned(),
                        message: format!("gateway websocket pong failed: {error}"),
                    })?;
            }
            Ok(WsMessage::Pong(_)) => {}
            Ok(WsMessage::Close(_)) => {
                return Err(RunnerError::GatewayProbeFailed {
                    endpoint: endpoint.to_owned(),
                    message: "gateway websocket closed during probe".to_owned(),
                });
            }
            Ok(_) => {
                return Err(RunnerError::GatewayProbeFailed {
                    endpoint: endpoint.to_owned(),
                    message: "unsupported websocket message type during probe".to_owned(),
                });
            }
            Err(error) => {
                return Err(RunnerError::GatewayProbeFailed {
                    endpoint: endpoint.to_owned(),
                    message: format!("gateway websocket receive failed: {error}"),
                });
            }
        }
    }
}

/// Sends a control request to a running runner daemon via its Unix control
/// socket and returns the daemon's response. Creates a short-lived tokio
/// runtime internally so it can be called from synchronous CLI code.
pub fn send_control_to_daemon(
    control_socket_path: &Path,
    request: &RunnerControl,
) -> Result<RunnerControlResponse, RunnerControlTransportError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(RunnerControlTransportError::Transport)?;
    rt.block_on(async {
        let mut stream = tokio::net::UnixStream::connect(control_socket_path)
            .await
            .map_err(RunnerControlTransportError::Transport)?;
        let payload = serde_json::to_vec(request)?;
        write_runner_control_frame(&mut stream, &payload).await?;
        let response_frame = read_runner_control_frame(&mut stream)
            .await?
            .ok_or_else(|| {
                RunnerControlTransportError::Transport(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "daemon closed connection without sending a response",
                ))
            })?;
        serde_json::from_slice(&response_frame).map_err(RunnerControlTransportError::Serialization)
    })
}

fn validate_user_id(user_id: &str) -> Result<&str, RunnerError> {
    let user_id = user_id.trim();
    if user_id.is_empty()
        || user_id.contains('/')
        || user_id.contains('\\')
        || user_id.contains("..")
    {
        return Err(RunnerError::InvalidUserId {
            user_id: user_id.to_owned(),
        });
    }

    Ok(user_id)
}
