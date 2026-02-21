use std::{
    collections::HashMap,
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

use bollard::{
    API_DEFAULT_VERSION, Docker,
    errors::Error as BollardError,
    models::{ContainerCreateBody, HostConfig},
    query_parameters::{
        CreateContainerOptionsBuilder, RemoveContainerOptionsBuilder, StopContainerOptionsBuilder,
    },
};
use futures_util::{SinkExt, StreamExt};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode, Uri, body::Bytes};
use hyper_util::client::legacy::Client;
use hyperlocal::{UnixClientExt, UnixConnector, Uri as HyperlocalUri};
use sandbox::{ProcessHardeningOutcome, attempt_process_tier_hardening};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message as WsMessage,
};
use tracing::{info, warn};
use types::{
    BootstrapEnvelopeError, GATEWAY_PROTOCOL_VERSION, GatewayClientFrame, GatewayClientHello,
    GatewayHealthCheck, GatewayServerFrame, RunnerBootstrapEnvelope, RunnerConfigError,
    RunnerGlobalConfig, RunnerGuestImages, RunnerUserConfig, RunnerUserRegistration, SandboxTier,
    SidecarEndpoint, SidecarTransport,
};

pub const PROCESS_TIER_WARNING: &str = "Process tier is insecure: isolation is degraded and not production-safe; shell/browser tools are disabled.";

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";
const GATEWAY_ENDPOINT_MARKER_FILE: &str = "gateway-endpoint";

const FIRECRACKER_BINARY: &str = "firecracker";
const PROCESS_EXECUTABLE_ENV_KEY: &str = "OXYDRA_VM_PROCESS_EXECUTABLE";
const DEFAULT_PROCESS_EXECUTABLE: &str = "oxydra-vm";
const DEFAULT_DOCKER_TIMEOUT_SECS: u64 = 120;

const DOCKER_SANDBOXD_SOCKET_RELATIVE_PATH: &str = ".docker/sandboxes/sandboxd.sock";
const DOCKER_SANDBOX_VM_ENDPOINT: &str = "/vm";
const FIRECRACKER_API_READY_TIMEOUT: Duration = Duration::from_secs(5);
const FIRECRACKER_API_READY_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
        let sandbox_tier =
            resolve_sandbox_tier(&self.global_config, &user_config, request.insecure);
        let capabilities = resolve_requested_capabilities(sandbox_tier, &user_config);

        let mut launch = self.backend.launch(SandboxLaunchRequest {
            user_id: user_id.clone(),
            host_os: host_os.to_owned(),
            sandbox_tier,
            workspace: workspace.clone(),
            guest_images: self.global_config.guest_images.clone(),
            requested_shell: capabilities.shell,
            requested_browser: capabilities.browser,
        })?;

        let sidecar_endpoint = if launch.shell_available || launch.browser_available {
            launch.sidecar_endpoint.take()
        } else {
            None
        };

        let bootstrap = RunnerBootstrapEnvelope {
            user_id: user_id.clone(),
            sandbox_tier,
            workspace_root: workspace.root.to_string_lossy().into_owned(),
            sidecar_endpoint,
        };
        bootstrap.validate()?;

        info!(
            user_id = %user_id,
            sandbox_tier = ?sandbox_tier,
            shell_available = launch.shell_available,
            browser_available = launch.browser_available,
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
            launch: launch.launch,
            bootstrap,
            warnings: launch.warnings,
        })
    }

    pub fn connect_tui(
        &self,
        request: RunnerTuiConnectRequest,
    ) -> Result<RunnerTuiConnection, RunnerError> {
        let user_id = validate_user_id(&request.user_id)?.to_owned();
        let _user_config = self.load_user_config(&user_id)?;
        let workspace = self.provision_user_workspace(&user_id)?;
        let endpoint_path = workspace.tmp.join(GATEWAY_ENDPOINT_MARKER_FILE);
        let gateway_endpoint = read_gateway_endpoint_marker(&endpoint_path, &user_id)?;
        let runtime_session_id = probe_gateway_health(&gateway_endpoint, &user_id)?;

        Ok(RunnerTuiConnection {
            user_id,
            workspace,
            gateway_endpoint,
            runtime_session_id,
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
    let shared = root.join(SHARED_DIR_NAME);
    let tmp = root.join(TMP_DIR_NAME);
    let vault = root.join(VAULT_DIR_NAME);

    for path in [&root, &shared, &tmp, &vault] {
        fs::create_dir_all(path).map_err(|source| RunnerError::ProvisionWorkspace {
            path: path.clone(),
            source,
        })?;
    }

    Ok(UserWorkspace {
        root,
        shared,
        tmp,
        vault,
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
        }
    }

    pub fn simulated(role: RunnerGuestRole, command: RunnerCommandSpec) -> Self {
        Self {
            role,
            command,
            pid: None,
            lifecycle: RunnerGuestLifecycle::Simulated,
        }
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
}

impl RunnerStartRequest {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self {
            user_id: user_id.into(),
            insecure: false,
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
    pub runtime_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserWorkspace {
    pub root: PathBuf,
    pub shared: PathBuf,
    pub tmp: PathBuf,
    pub vault: PathBuf,
}

#[derive(Debug)]
pub struct RunnerStartup {
    pub user_id: String,
    pub sandbox_tier: SandboxTier,
    pub workspace: UserWorkspace,
    pub shell_available: bool,
    pub browser_available: bool,
    pub launch: RunnerLaunchHandle,
    pub bootstrap: RunnerBootstrapEnvelope,
    pub warnings: Vec<String>,
}

impl RunnerStartup {
    pub fn shutdown(&mut self) -> Result<(), RunnerError> {
        self.launch.shutdown()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLaunchRequest {
    pub user_id: String,
    pub host_os: String,
    pub sandbox_tier: SandboxTier,
    pub workspace: UserWorkspace,
    pub guest_images: RunnerGuestImages,
    pub requested_shell: bool,
    pub requested_browser: bool,
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
    pub warnings: Vec<String>,
}

pub trait SandboxBackend: Send + Sync + std::fmt::Debug {
    fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError>;
}

#[derive(Debug, Default)]
pub struct CrateSandboxBackend;

pub type CommandSandboxBackend = CrateSandboxBackend;

impl SandboxBackend for CrateSandboxBackend {
    fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError> {
        match request.sandbox_tier {
            SandboxTier::MicroVm => match request.host_os.as_str() {
                "linux" => self.launch_microvm_linux(&request),
                "macos" => self.launch_microvm_macos(&request),
                _ => Err(RunnerError::UnsupportedMicroVmHost {
                    os: request.host_os.clone(),
                }),
            },
            SandboxTier::Container => self.launch_container(&request),
            SandboxTier::Process => self.launch_process(),
        }
    }
}

impl CrateSandboxBackend {
    fn launch_microvm_linux(
        &self,
        request: &SandboxLaunchRequest,
    ) -> Result<SandboxLaunch, RunnerError> {
        let runtime_api_socket = request.workspace.tmp.join("oxydra-vm-firecracker.sock");
        let runtime_command = RunnerCommandSpec::new(
            FIRECRACKER_BINARY,
            vec![
                "--api-sock".to_owned(),
                runtime_api_socket.to_string_lossy().into_owned(),
                "--config-file".to_owned(),
                request.guest_images.oxydra_vm.clone(),
            ],
        );
        let mut runtime = self.spawn_process_guest(RunnerGuestRole::OxydraVm, runtime_command)?;
        if let Err(error) = ensure_firecracker_api_ready(runtime_api_socket.clone()) {
            let _ = runtime.shutdown();
            return Err(error);
        }

        let mut warnings = Vec::new();
        let mut sidecar = None;
        if request.sidecar_requested() {
            let sidecar_api_socket = request.workspace.tmp.join("shell-vm-firecracker.sock");
            let sidecar_command = RunnerCommandSpec::new(
                FIRECRACKER_BINARY,
                vec![
                    "--api-sock".to_owned(),
                    sidecar_api_socket.to_string_lossy().into_owned(),
                    "--config-file".to_owned(),
                    request.guest_images.shell_vm.clone(),
                ],
            );
            match self.spawn_process_guest(RunnerGuestRole::ShellVm, sidecar_command) {
                Ok(mut handle) => {
                    if let Err(error) = ensure_firecracker_api_ready(sidecar_api_socket) {
                        let _ = handle.shutdown();
                        warnings.push(format!("linux microvm sidecar launch failed: {error}"));
                    } else {
                        sidecar = Some(handle);
                    }
                }
                Err(error) => {
                    warnings.push(format!("linux microvm sidecar launch failed: {error}"))
                }
            }
        }

        let sidecar_endpoint = sidecar.as_ref().map(|_| SidecarEndpoint {
            transport: SidecarTransport::Vsock,
            address: format!(
                "unix://{}",
                request
                    .workspace
                    .tmp
                    .join("shell-daemon-vsock.sock")
                    .to_string_lossy()
            ),
        });
        let shell_available = request.requested_shell && sidecar.is_some();
        let browser_available = request.requested_browser && sidecar.is_some();

        Ok(SandboxLaunch {
            launch: RunnerLaunchHandle {
                tier: SandboxTier::MicroVm,
                runtime,
                sidecar,
                scope: None,
            },
            sidecar_endpoint,
            shell_available,
            browser_available,
            warnings,
        })
    }

    fn launch_microvm_macos(
        &self,
        request: &SandboxLaunchRequest,
    ) -> Result<SandboxLaunch, RunnerError> {
        let vm_name = format!(
            "oxydra-microvm-{}",
            sanitize_container_component(&request.user_id)
        );
        let sandbox_socket_path = docker_sandboxd_socket_path()?;
        let vm_info = create_docker_sandbox_vm_sync(sandbox_socket_path.clone(), vm_name.clone())?;
        let docker_endpoint =
            DockerEndpoint::UnixSocket(vm_info.docker_socket_path.to_string_lossy().into_owned());

        let runtime = match self.launch_docker_guest(
            &docker_endpoint,
            request,
            "micro_vm",
            RunnerGuestRole::OxydraVm,
            &request.guest_images.oxydra_vm,
        ) {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = delete_docker_sandbox_vm_sync(sandbox_socket_path, vm_name);
                return Err(error);
            }
        };

        let mut warnings = Vec::new();
        let mut sidecar = None;
        if request.sidecar_requested() {
            match self.launch_docker_guest(
                &docker_endpoint,
                request,
                "micro_vm",
                RunnerGuestRole::ShellVm,
                &request.guest_images.shell_vm,
            ) {
                Ok(handle) => sidecar = Some(handle),
                Err(error) => {
                    warnings.push(format!("macOS microvm sidecar launch failed: {error}"))
                }
            }
        }

        let sidecar_endpoint = sidecar.as_ref().map(|_| SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: request
                .workspace
                .tmp
                .join("shell-daemon.sock")
                .to_string_lossy()
                .into_owned(),
        });
        let shell_available = request.requested_shell && sidecar.is_some();
        let browser_available = request.requested_browser && sidecar.is_some();

        Ok(SandboxLaunch {
            launch: RunnerLaunchHandle {
                tier: SandboxTier::MicroVm,
                runtime,
                sidecar,
                scope: Some(RunnerScopeHandle::DockerSandboxVm(DockerSandboxVmHandle {
                    vm_name: vm_info.vm_name,
                    sandbox_socket_path,
                })),
            },
            sidecar_endpoint,
            shell_available,
            browser_available,
            warnings,
        })
    }

    fn launch_container(
        &self,
        request: &SandboxLaunchRequest,
    ) -> Result<SandboxLaunch, RunnerError> {
        let docker_endpoint = DockerEndpoint::Local;
        let runtime = self.launch_docker_guest(
            &docker_endpoint,
            request,
            "container",
            RunnerGuestRole::OxydraVm,
            &request.guest_images.oxydra_vm,
        )?;

        let mut warnings = Vec::new();
        let mut sidecar = None;
        if request.sidecar_requested() {
            match self.launch_docker_guest(
                &docker_endpoint,
                request,
                "container",
                RunnerGuestRole::ShellVm,
                &request.guest_images.shell_vm,
            ) {
                Ok(handle) => sidecar = Some(handle),
                Err(error) => warnings.push(format!("container sidecar launch failed: {error}")),
            }
        }

        let sidecar_endpoint = sidecar.as_ref().map(|_| SidecarEndpoint {
            transport: SidecarTransport::Unix,
            address: request
                .workspace
                .tmp
                .join("shell-daemon.sock")
                .to_string_lossy()
                .into_owned(),
        });
        let shell_available = request.requested_shell && sidecar.is_some();
        let browser_available = request.requested_browser && sidecar.is_some();

        Ok(SandboxLaunch {
            launch: RunnerLaunchHandle {
                tier: SandboxTier::Container,
                runtime,
                sidecar,
                scope: None,
            },
            sidecar_endpoint,
            shell_available,
            browser_available,
            warnings,
        })
    }

    fn launch_process(&self) -> Result<SandboxLaunch, RunnerError> {
        let executable = std::env::var(PROCESS_EXECUTABLE_ENV_KEY)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_PROCESS_EXECUTABLE.to_owned());
        let runtime_command = RunnerCommandSpec::new(executable, Vec::new());
        let runtime = self.spawn_process_guest(RunnerGuestRole::OxydraVm, runtime_command)?;
        let hardening_attempt = attempt_process_tier_hardening();

        let mut warnings = vec![PROCESS_TIER_WARNING.to_owned()];
        if hardening_attempt.outcome != ProcessHardeningOutcome::Success {
            warnings.push(format!(
                "process-tier hardening {:?}: {}",
                hardening_attempt.outcome, hardening_attempt.detail
            ));
        }

        Ok(SandboxLaunch {
            launch: RunnerLaunchHandle {
                tier: SandboxTier::Process,
                runtime,
                sidecar: None,
                scope: None,
            },
            sidecar_endpoint: None,
            shell_available: false,
            browser_available: false,
            warnings,
        })
    }

    fn launch_docker_guest(
        &self,
        endpoint: &DockerEndpoint,
        request: &SandboxLaunchRequest,
        tier_label: &str,
        role: RunnerGuestRole,
        image: &str,
    ) -> Result<RunnerGuestHandle, RunnerError> {
        let container_name = docker_guest_container_name(tier_label, &request.user_id, role);
        let command = docker_guest_command_spec(endpoint, &container_name);
        let labels = docker_guest_labels(tier_label, &request.user_id, role);

        run_async(launch_docker_container_async(
            endpoint.clone(),
            container_name.clone(),
            image.to_owned(),
            request.workspace.clone(),
            labels,
        ))?;

        Ok(RunnerGuestHandle::for_docker(
            role,
            command,
            endpoint.clone(),
            container_name,
        ))
    }

    fn spawn_process_guest(
        &self,
        role: RunnerGuestRole,
        command: RunnerCommandSpec,
    ) -> Result<RunnerGuestHandle, RunnerError> {
        let mut child_command = Command::new(&command.program);
        child_command
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = child_command
            .spawn()
            .map_err(|source| RunnerError::LaunchGuest {
                role,
                program: command.program.clone(),
                source,
            })?;
        Ok(RunnerGuestHandle::from_child(role, command, child))
    }
}

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
}

#[derive(Debug, Clone, Copy)]
struct RequestedCapabilities {
    shell: bool,
    browser: bool,
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

async fn launch_docker_container_async(
    endpoint: DockerEndpoint,
    container_name: String,
    image: String,
    workspace: UserWorkspace,
    labels: HashMap<String, String>,
) -> Result<(), RunnerError> {
    let docker = docker_client(&endpoint)?;
    remove_container_if_exists(&docker, &endpoint, &container_name).await?;

    let binds = vec![format!(
        "{}:{}",
        workspace.root.to_string_lossy(),
        workspace.root.to_string_lossy()
    )];
    let config = ContainerCreateBody {
        image: Some(image),
        labels: Some(labels),
        host_config: Some(HostConfig {
            binds: Some(binds),
            ..HostConfig::default()
        }),
        ..ContainerCreateBody::default()
    };

    docker
        .create_container(
            Some(
                CreateContainerOptionsBuilder::new()
                    .name(&container_name)
                    .build(),
            ),
            config,
        )
        .await
        .map_err(|source| {
            docker_operation_error(&endpoint, "create_container", &container_name, source)
        })?;

    docker
        .start_container(
            &container_name,
            None::<bollard::query_parameters::StartContainerOptions>,
        )
        .await
        .map_err(|source| {
            docker_operation_error(&endpoint, "start_container", &container_name, source)
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
    vm_name: String,
) -> Result<DockerSandboxVmInfo, RunnerError> {
    run_async(async move {
        let _ = delete_docker_sandbox_vm_async(sandbox_socket_path.clone(), vm_name.clone()).await;
        let payload = json!({ "name": vm_name });
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
                vm_name: vm_name.clone(),
            },
        )?;

        Ok(DockerSandboxVmInfo {
            vm_name,
            docker_socket_path,
        })
    })
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
                runtime_session_id: None,
            }),
            &gateway_endpoint,
        )
        .await?;

        let runtime_session_id =
            match receive_gateway_server_frame(&mut socket, &gateway_endpoint).await? {
                GatewayServerFrame::HelloAck(ack) => ack.session.runtime_session_id,
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
                    let _ = socket.close(None).await;
                    return Ok(runtime_session_id);
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

#[cfg(test)]
mod tests {
    use std::{
        env, fs,
        net::TcpListener,
        path::{Path, PathBuf},
        sync::Mutex,
        thread,
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio_tungstenite::tungstenite::{Message as WsMessage, accept};

    use super::*;

    #[derive(Debug, Default)]
    struct MockSandboxBackend {
        launches: Mutex<Vec<SandboxLaunchRequest>>,
    }

    impl MockSandboxBackend {
        fn recorded_launches(&self) -> Vec<SandboxLaunchRequest> {
            self.launches
                .lock()
                .expect("launch records mutex should not be poisoned")
                .clone()
        }
    }

    impl SandboxBackend for MockSandboxBackend {
        fn launch(&self, request: SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError> {
            self.launches
                .lock()
                .expect("launch records mutex should not be poisoned")
                .push(request.clone());

            let runtime = RunnerGuestHandle::simulated(
                RunnerGuestRole::OxydraVm,
                RunnerCommandSpec::new("mock-oxydra-vm", Vec::new()),
            );

            let transport = match request.sandbox_tier {
                SandboxTier::MicroVm if request.host_os == "linux" => SidecarTransport::Vsock,
                SandboxTier::MicroVm | SandboxTier::Container => SidecarTransport::Unix,
                SandboxTier::Process => SidecarTransport::Unix,
            };
            let sidecar_requested = request.sidecar_requested();
            let sidecar = sidecar_requested.then(|| {
                RunnerGuestHandle::simulated(
                    RunnerGuestRole::ShellVm,
                    RunnerCommandSpec::new("mock-shell-vm", Vec::new()),
                )
            });
            let sidecar_endpoint = sidecar_requested.then(|| SidecarEndpoint {
                transport,
                address: "/tmp/mock-shell-daemon.sock".to_owned(),
            });

            let mut warnings = Vec::new();
            if request.sandbox_tier == SandboxTier::Process {
                warnings.push(PROCESS_TIER_WARNING.to_owned());
            }

            Ok(SandboxLaunch {
                launch: RunnerLaunchHandle {
                    tier: request.sandbox_tier,
                    runtime,
                    sidecar,
                    scope: Some(RunnerScopeHandle::Simulated),
                },
                sidecar_endpoint,
                shell_available: sidecar_requested && request.requested_shell,
                browser_available: sidecar_requested && request.requested_browser,
                warnings,
            })
        }
    }

    #[test]
    fn global_and_user_configs_load_with_validation() {
        let root = temp_dir("config-load");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let global = load_runner_global_config(&global_path).expect("global config should load");
        let user = load_runner_user_config(root.join("users/alice.toml"))
            .expect("user config should load");
        assert_eq!(global.workspace_root, "workspaces");
        assert_eq!(global.default_tier, SandboxTier::MicroVm);
        assert_eq!(user, RunnerUserConfig::default());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn runner_resolves_relative_user_config_paths() {
        let root = temp_dir("path-resolution");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(
            &root.join("users/alice.toml"),
            "[behavior]\nsandbox_tier = \"micro_vm\"\n",
        );

        let backend: Arc<dyn SandboxBackend> = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend)
            .expect("runner should initialize");
        let user_config = runner
            .load_user_config("alice")
            .expect("user config should load through registration map");
        assert_eq!(
            user_config.behavior.sandbox_tier,
            Some(SandboxTier::MicroVm)
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_provisioning_creates_expected_directories() {
        let root = temp_dir("workspace-provisioning");
        let workspace = provision_user_workspace(root.join("workspace-root"), "alice")
            .expect("workspace should provision");
        assert!(workspace.root.is_dir());
        assert!(workspace.shared.is_dir());
        assert!(workspace.tmp.is_dir());
        assert!(workspace.vault.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sandbox_tier_resolution_honors_insecure_override() {
        let global = RunnerGlobalConfig {
            default_tier: SandboxTier::Container,
            ..RunnerGlobalConfig::default()
        };
        global.validate().expect("global config should be valid");

        let mut user = RunnerUserConfig::default();
        user.behavior.sandbox_tier = Some(SandboxTier::MicroVm);

        assert_eq!(
            resolve_sandbox_tier(&global, &user, false),
            SandboxTier::MicroVm
        );
        assert_eq!(
            resolve_sandbox_tier(&global, &user, true),
            SandboxTier::Process
        );
    }

    #[test]
    fn startup_uses_linux_microvm_backend_and_vsock_sidecar() {
        let root = temp_dir("linux-microvm");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
        assert_eq!(startup.launch.tier, SandboxTier::MicroVm);
        assert_eq!(
            startup
                .bootstrap
                .sidecar_endpoint
                .as_ref()
                .map(|sidecar| sidecar.transport),
            Some(SidecarTransport::Vsock)
        );
        assert!(startup.shell_available);
        assert!(startup.browser_available);
        assert!(startup.warnings.is_empty());

        let launches = backend.recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].sandbox_tier, SandboxTier::MicroVm);
        assert!(launches[0].requested_shell);
        assert!(launches[0].requested_browser);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_uses_macos_microvm_backend_with_unix_sidecar() {
        let root = temp_dir("macos-microvm");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "macos")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
        assert_eq!(startup.launch.tier, SandboxTier::MicroVm);
        assert_eq!(
            startup
                .bootstrap
                .sidecar_endpoint
                .as_ref()
                .map(|sidecar| sidecar.transport),
            Some(SidecarTransport::Unix)
        );
        assert!(startup.shell_available);
        assert!(startup.browser_available);

        let launches = backend.recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].sandbox_tier, SandboxTier::MicroVm);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_insecure_process_mode_disables_shell_and_browser() {
        let root = temp_dir("process-mode");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let startup = runner
            .start_user_for_host(
                RunnerStartRequest {
                    user_id: "alice".to_owned(),
                    insecure: true,
                },
                "linux",
            )
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::Process);
        assert_eq!(startup.launch.tier, SandboxTier::Process);
        assert!(startup.launch.sidecar.is_none());
        assert!(!startup.shell_available);
        assert!(!startup.browser_available);
        assert!(startup.bootstrap.sidecar_endpoint.is_none());
        assert_eq!(startup.warnings, vec![PROCESS_TIER_WARNING.to_owned()]);

        let launches = backend.recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].sandbox_tier, SandboxTier::Process);
        assert!(!launches[0].requested_shell);
        assert!(!launches[0].requested_browser);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_container_tier_uses_unix_sidecar_transport() {
        let root = temp_dir("container-tier");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::Container);
        assert_eq!(startup.launch.tier, SandboxTier::Container);
        assert_eq!(
            startup
                .bootstrap
                .sidecar_endpoint
                .as_ref()
                .map(|sidecar| sidecar.transport),
            Some(SidecarTransport::Unix)
        );

        let launches = backend.recorded_launches();
        assert_eq!(launches.len(), 1);
        assert_eq!(launches[0].sandbox_tier, SandboxTier::Container);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tui_connect_only_succeeds_against_running_gateway_endpoint() {
        let root = temp_dir("tui-connect-success");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let workspace = runner
            .provision_user_workspace("alice")
            .expect("workspace should provision");

        let (gateway_endpoint, server_task) = spawn_mock_gateway_probe_server(true);
        fs::write(
            workspace.tmp.join(GATEWAY_ENDPOINT_MARKER_FILE),
            &gateway_endpoint,
        )
        .expect("gateway endpoint marker should be writable");

        let connection = runner
            .connect_tui(RunnerTuiConnectRequest::new("alice"))
            .expect("connect-only path should succeed with running gateway");
        assert_eq!(connection.user_id, "alice");
        assert_eq!(connection.workspace.root, workspace.root);
        assert_eq!(connection.gateway_endpoint, gateway_endpoint);
        assert_eq!(connection.runtime_session_id, "runtime-alice");
        assert!(backend.recorded_launches().is_empty());

        server_task.join().expect("mock gateway should shut down");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tui_connect_only_fails_with_clear_error_when_guest_is_absent() {
        let root = temp_dir("tui-connect-missing-guest");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");

        let error = runner
            .connect_tui(RunnerTuiConnectRequest::new("alice"))
            .expect_err("missing running guest should fail connect-only path");
        assert!(matches!(
            error,
            RunnerError::NoRunningGuest {
                ref user_id,
                ref endpoint_path
            } if user_id == "alice" && endpoint_path.ends_with(GATEWAY_ENDPOINT_MARKER_FILE)
        ));
        assert!(backend.recorded_launches().is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn tui_connect_only_never_spawns_guest_when_probe_fails() {
        let root = temp_dir("tui-connect-probe-fail");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(&root.join("users/alice.toml"), "");

        let backend = Arc::new(MockSandboxBackend::default());
        let runner = Runner::from_global_config_path_with_backend(&global_path, backend.clone())
            .expect("runner should initialize");
        let workspace = runner
            .provision_user_workspace("alice")
            .expect("workspace should provision");
        fs::write(
            workspace.tmp.join(GATEWAY_ENDPOINT_MARKER_FILE),
            "ws://127.0.0.1:9/ws",
        )
        .expect("gateway endpoint marker should be writable");

        let error = runner
            .connect_tui(RunnerTuiConnectRequest::new("alice"))
            .expect_err("unreachable gateway probe should fail");
        assert!(matches!(error, RunnerError::GatewayProbeFailed { .. }));
        assert!(backend.recorded_launches().is_empty());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_rejects_microvm_on_unsupported_host() {
        let root = temp_dir("unsupported-host");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
        let error = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "windows")
            .expect_err("unsupported host should fail");
        assert!(matches!(error, RunnerError::UnsupportedMicroVmHost { .. }));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn extract_socket_path_handles_nested_payload() {
        let payload = json!({
            "vm": {
                "connection": {
                    "socketPath": "/tmp/sandbox/docker.sock"
                }
            }
        });
        assert_eq!(
            extract_socket_path(&payload),
            Some(PathBuf::from("/tmp/sandbox/docker.sock"))
        );
    }

    fn spawn_mock_gateway_probe_server(healthy: bool) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock gateway should bind");
        let address = listener
            .local_addr()
            .expect("mock gateway should expose address");
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().expect("mock gateway should accept");
            let mut socket = accept(stream).expect("mock websocket handshake should succeed");

            let hello = parse_client_frame(
                socket
                    .read()
                    .expect("mock gateway should receive hello frame"),
            );
            let user_id = match hello {
                GatewayClientFrame::Hello(hello) => hello.user_id,
                other => panic!("expected hello frame, got {other:?}"),
            };
            send_server_frame(
                &mut socket,
                GatewayServerFrame::HelloAck(types::GatewayHelloAck {
                    request_id: "runner-connect-hello".to_owned(),
                    protocol_version: GATEWAY_PROTOCOL_VERSION,
                    session: types::GatewaySession {
                        user_id: user_id.clone(),
                        runtime_session_id: format!("runtime-{user_id}"),
                    },
                    active_turn: None,
                }),
            );

            let health_check = parse_client_frame(
                socket
                    .read()
                    .expect("mock gateway should receive health check"),
            );
            let request_id = match health_check {
                GatewayClientFrame::HealthCheck(request) => request.request_id,
                other => panic!("expected health_check frame, got {other:?}"),
            };
            send_server_frame(
                &mut socket,
                GatewayServerFrame::HealthStatus(types::GatewayHealthStatus {
                    request_id,
                    healthy,
                    session: Some(types::GatewaySession {
                        user_id: user_id.clone(),
                        runtime_session_id: format!("runtime-{user_id}"),
                    }),
                    active_turn: None,
                    message: Some(if healthy {
                        "ready".to_owned()
                    } else {
                        "unhealthy".to_owned()
                    }),
                }),
            );
            let _ = socket.close(None);
        });

        (format!("ws://{address}/ws"), handle)
    }

    fn parse_client_frame(message: WsMessage) -> GatewayClientFrame {
        match message {
            WsMessage::Text(payload) => {
                serde_json::from_str::<GatewayClientFrame>(payload.as_ref())
                    .expect("mock gateway should decode text client frame")
            }
            WsMessage::Binary(payload) => serde_json::from_slice::<GatewayClientFrame>(&payload)
                .expect("mock gateway should decode binary client frame"),
            other => panic!("unexpected client websocket message: {other:?}"),
        }
    }

    fn send_server_frame(
        socket: &mut tokio_tungstenite::tungstenite::WebSocket<std::net::TcpStream>,
        frame: GatewayServerFrame,
    ) {
        let payload = serde_json::to_string(&frame).expect("mock server frame should encode");
        socket
            .send(WsMessage::Text(payload.into()))
            .expect("mock gateway should send server frame");
    }

    fn write_runner_config_fixture(root: &Path, default_tier: &str) -> PathBuf {
        let path = root.join("runner.toml");
        fs::create_dir_all(root).expect("root should exist");
        fs::write(
            &path,
            format!(
                r#"
workspace_root = "workspaces"
default_tier = "{default_tier}"

[guest_images]
oxydra_vm = "oxydra-vm:test"
shell_vm = "shell-vm:test"

[users.alice]
config_path = "users/alice.toml"
"#
            )
            .trim_start(),
        )
        .expect("runner config should be writable");
        path
    }

    fn write_user_config(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent directory should exist");
        }
        fs::write(path, content).expect("user config should be writable");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        path.push(format!(
            "oxydra-runner-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }
}
