use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tracing::{info, warn};
use types::{
    BootstrapEnvelopeError, RunnerBootstrapEnvelope, RunnerConfigError, RunnerGlobalConfig,
    RunnerGuestImages, RunnerUserConfig, RunnerUserRegistration, SandboxTier, SidecarEndpoint,
    SidecarTransport,
};

pub const PROCESS_TIER_WARNING: &str = "Process tier is insecure: isolation is degraded and not production-safe; shell/browser tools are disabled.";

const SHARED_DIR_NAME: &str = "shared";
const TMP_DIR_NAME: &str = "tmp";
const VAULT_DIR_NAME: &str = "vault";

#[derive(Debug, Clone)]
pub struct Runner {
    global_config: RunnerGlobalConfig,
    global_config_path: PathBuf,
}

impl Runner {
    pub fn from_global_config_path(path: impl AsRef<Path>) -> Result<Self, RunnerError> {
        let global_config_path = path.as_ref().to_path_buf();
        let global_config = load_runner_global_config(&global_config_path)?;
        Ok(Self {
            global_config,
            global_config_path,
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

        let LaunchArtifacts {
            launch_plan,
            sidecar_endpoint,
            mut shell_available,
            mut browser_available,
            warnings,
        } = build_launch_artifacts(
            sandbox_tier,
            &self.global_config.guest_images,
            &workspace,
            &user_id,
            host_os,
        )?;

        if let Some(enabled) = user_config.behavior.shell_enabled {
            shell_available &= enabled;
        }
        if let Some(enabled) = user_config.behavior.browser_enabled {
            browser_available &= enabled;
        }

        let sidecar_endpoint = if shell_available || browser_available {
            sidecar_endpoint
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
            shell_available,
            browser_available,
            "runner startup prepared"
        );
        for warning_message in &warnings {
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
            shell_available,
            browser_available,
            launch_plan,
            bootstrap,
            warnings,
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
pub struct UserWorkspace {
    pub root: PathBuf,
    pub shared: PathBuf,
    pub tmp: PathBuf,
    pub vault: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerStartup {
    pub user_id: String,
    pub sandbox_tier: SandboxTier,
    pub workspace: UserWorkspace,
    pub shell_available: bool,
    pub browser_available: bool,
    pub launch_plan: RunnerLaunchPlan,
    pub bootstrap: RunnerBootstrapEnvelope,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerLaunchPlan {
    MicroVmLinux {
        oxydra_vm_image: String,
        shell_vm_image: String,
        transport: SidecarTransport,
    },
    MicroVmMacOs {
        oxydra_vm_image: String,
        shell_vm_image: String,
        transport: SidecarTransport,
    },
    Container {
        oxydra_vm_image: String,
        shell_vm_image: String,
        transport: SidecarTransport,
    },
    Process {
        executable: String,
    },
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
    #[error("failed to provision workspace directory `{path}`: {source}")]
    ProvisionWorkspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone)]
struct LaunchArtifacts {
    launch_plan: RunnerLaunchPlan,
    sidecar_endpoint: Option<SidecarEndpoint>,
    shell_available: bool,
    browser_available: bool,
    warnings: Vec<String>,
}

fn build_launch_artifacts(
    sandbox_tier: SandboxTier,
    guest_images: &RunnerGuestImages,
    workspace: &UserWorkspace,
    user_id: &str,
    host_os: &str,
) -> Result<LaunchArtifacts, RunnerError> {
    match sandbox_tier {
        SandboxTier::MicroVm => match host_os {
            "linux" => Ok(LaunchArtifacts {
                launch_plan: RunnerLaunchPlan::MicroVmLinux {
                    oxydra_vm_image: guest_images.oxydra_vm.clone(),
                    shell_vm_image: guest_images.shell_vm.clone(),
                    transport: SidecarTransport::Vsock,
                },
                sidecar_endpoint: Some(SidecarEndpoint {
                    transport: SidecarTransport::Vsock,
                    address: format!("vsock://{user_id}/shell-daemon"),
                }),
                shell_available: true,
                browser_available: true,
                warnings: Vec::new(),
            }),
            "macos" => Ok(LaunchArtifacts {
                launch_plan: RunnerLaunchPlan::MicroVmMacOs {
                    oxydra_vm_image: guest_images.oxydra_vm.clone(),
                    shell_vm_image: guest_images.shell_vm.clone(),
                    transport: SidecarTransport::Unix,
                },
                sidecar_endpoint: Some(SidecarEndpoint {
                    transport: SidecarTransport::Unix,
                    address: workspace
                        .tmp
                        .join("shell-daemon.sock")
                        .to_string_lossy()
                        .into_owned(),
                }),
                shell_available: true,
                browser_available: true,
                warnings: Vec::new(),
            }),
            _ => Err(RunnerError::UnsupportedMicroVmHost {
                os: host_os.to_owned(),
            }),
        },
        SandboxTier::Container => Ok(LaunchArtifacts {
            launch_plan: RunnerLaunchPlan::Container {
                oxydra_vm_image: guest_images.oxydra_vm.clone(),
                shell_vm_image: guest_images.shell_vm.clone(),
                transport: SidecarTransport::Unix,
            },
            sidecar_endpoint: Some(SidecarEndpoint {
                transport: SidecarTransport::Unix,
                address: workspace
                    .tmp
                    .join("shell-daemon.sock")
                    .to_string_lossy()
                    .into_owned(),
            }),
            shell_available: true,
            browser_available: true,
            warnings: Vec::new(),
        }),
        SandboxTier::Process => Ok(LaunchArtifacts {
            launch_plan: RunnerLaunchPlan::Process {
                executable: "oxydra-vm".to_owned(),
            },
            sidecar_endpoint: None,
            shell_available: false,
            browser_available: false,
            warnings: vec![PROCESS_TIER_WARNING.to_owned()],
        }),
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
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

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

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
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
    fn startup_uses_linux_microvm_launch_plan() {
        let root = temp_dir("linux-microvm");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
        assert!(matches!(
            startup.launch_plan,
            RunnerLaunchPlan::MicroVmLinux { .. }
        ));
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

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_uses_macos_microvm_launch_plan() {
        let root = temp_dir("macos-microvm");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "macos")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::MicroVm);
        assert!(matches!(
            startup.launch_plan,
            RunnerLaunchPlan::MicroVmMacOs { .. }
        ));
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

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_insecure_process_mode_disables_shell_and_browser() {
        let root = temp_dir("process-mode");
        let global_path = write_runner_config_fixture(&root, "micro_vm");
        write_user_config(&root.join("users/alice.toml"), "");

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
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
        assert!(matches!(
            startup.launch_plan,
            RunnerLaunchPlan::Process { .. }
        ));
        assert!(!startup.shell_available);
        assert!(!startup.browser_available);
        assert!(startup.bootstrap.sidecar_endpoint.is_none());
        assert_eq!(startup.warnings, vec![PROCESS_TIER_WARNING.to_owned()]);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn startup_container_tier_uses_unix_sidecar_transport() {
        let root = temp_dir("container-tier");
        let global_path = write_runner_config_fixture(&root, "container");
        write_user_config(&root.join("users/alice.toml"), "");

        let runner =
            Runner::from_global_config_path(&global_path).expect("runner should initialize");
        let startup = runner
            .start_user_for_host(RunnerStartRequest::new("alice"), "linux")
            .expect("startup should succeed");

        assert_eq!(startup.sandbox_tier, SandboxTier::Container);
        assert!(matches!(
            startup.launch_plan,
            RunnerLaunchPlan::Container { .. }
        ));
        assert_eq!(
            startup
                .bootstrap
                .sidecar_endpoint
                .as_ref()
                .map(|sidecar| sidecar.transport),
            Some(SidecarTransport::Unix)
        );

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
