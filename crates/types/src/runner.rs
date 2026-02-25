use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_RUNNER_WORKSPACE_ROOT: &str = ".oxydra/workspaces";
pub const DEFAULT_OXYDRA_VM_IMAGE: &str = "oxydra-vm:latest";
pub const DEFAULT_SHELL_VM_IMAGE: &str = "shell-vm:latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SandboxTier {
    #[default]
    MicroVm,
    Container,
    Process,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerGlobalConfig {
    #[serde(default = "default_runner_workspace_root")]
    pub workspace_root: String,
    #[serde(default)]
    pub users: BTreeMap<String, RunnerUserRegistration>,
    #[serde(default)]
    pub default_tier: SandboxTier,
    #[serde(default)]
    pub guest_images: RunnerGuestImages,
}

impl Default for RunnerGlobalConfig {
    fn default() -> Self {
        Self {
            workspace_root: default_runner_workspace_root(),
            users: BTreeMap::new(),
            default_tier: SandboxTier::default(),
            guest_images: RunnerGuestImages::default(),
        }
    }
}

impl RunnerGlobalConfig {
    pub fn validate(&self) -> Result<(), RunnerConfigError> {
        if self.workspace_root.trim().is_empty() {
            return Err(RunnerConfigError::InvalidWorkspaceRoot);
        }

        self.guest_images.validate()?;

        for (user_id, registration) in &self.users {
            if user_id.trim().is_empty() {
                return Err(RunnerConfigError::InvalidUserId);
            }
            if registration.config_path.trim().is_empty() {
                return Err(RunnerConfigError::InvalidUserConfigPath {
                    user_id: user_id.clone(),
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerUserRegistration {
    pub config_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerGuestImages {
    #[serde(default = "default_oxydra_vm_image")]
    pub oxydra_vm: String,
    #[serde(default = "default_shell_vm_image")]
    pub shell_vm: String,
    /// Firecracker JSON config file path for oxydra-vm (Linux microvm only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker_oxydra_vm_config: Option<String>,
    /// Firecracker JSON config file path for shell-vm (Linux microvm only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firecracker_shell_vm_config: Option<String>,
}

impl Default for RunnerGuestImages {
    fn default() -> Self {
        Self {
            oxydra_vm: default_oxydra_vm_image(),
            shell_vm: default_shell_vm_image(),
            firecracker_oxydra_vm_config: None,
            firecracker_shell_vm_config: None,
        }
    }
}

impl RunnerGuestImages {
    fn validate(&self) -> Result<(), RunnerConfigError> {
        if self.oxydra_vm.trim().is_empty() {
            return Err(RunnerConfigError::InvalidGuestImageRef { image: "oxydra_vm" });
        }
        if self.shell_vm.trim().is_empty() {
            return Err(RunnerConfigError::InvalidGuestImageRef { image: "shell_vm" });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerUserConfig {
    #[serde(default)]
    pub mounts: RunnerMountPaths,
    #[serde(default)]
    pub resources: RunnerResourceLimits,
    #[serde(default)]
    pub credential_refs: BTreeMap<String, String>,
    #[serde(default)]
    pub behavior: RunnerBehaviorOverrides,
    #[serde(default)]
    pub channels: ChannelsConfig,
}

impl RunnerUserConfig {
    pub fn validate(&self) -> Result<(), RunnerConfigError> {
        self.mounts.validate()?;
        self.resources.validate()?;
        validate_credential_refs(&self.credential_refs)?;

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerMountPaths {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<String>,
}

impl RunnerMountPaths {
    fn validate(&self) -> Result<(), RunnerConfigError> {
        validate_optional_path("shared", self.shared.as_deref())?;
        validate_optional_path("tmp", self.tmp.as_deref())?;
        validate_optional_path("vault", self.vault.as_deref())?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerResolvedMountPaths {
    pub shared: String,
    pub tmp: String,
    pub vault: String,
}

impl RunnerResolvedMountPaths {
    pub fn validate(&self) -> Result<(), RunnerConfigError> {
        validate_required_path("shared", &self.shared)?;
        validate_required_path("tmp", &self.tmp)?;
        validate_required_path("vault", &self.vault)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerRuntimePolicy {
    pub mounts: RunnerResolvedMountPaths,
    #[serde(default)]
    pub resources: RunnerResourceLimits,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub credential_refs: BTreeMap<String, String>,
}

impl RunnerRuntimePolicy {
    pub fn validate(&self) -> Result<(), RunnerConfigError> {
        self.mounts.validate()?;
        self.resources.validate()?;
        validate_credential_refs(&self.credential_refs)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupDegradedReasonCode {
    InsecureProcessTier,
    ProcessHardeningLimited,
    SidecarUnavailable,
    SidecarTransportUnsupported,
    SidecarEndpointInvalid,
    SidecarConnectionFailed,
    SidecarProtocolError,
    RuntimeShutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupDegradedReason {
    pub code: StartupDegradedReasonCode,
    pub detail: String,
}

impl StartupDegradedReason {
    pub fn new(code: StartupDegradedReasonCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupStatusReport {
    pub sandbox_tier: SandboxTier,
    pub sidecar_available: bool,
    pub shell_available: bool,
    pub browser_available: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<StartupDegradedReason>,
}

impl StartupStatusReport {
    pub fn has_reason_code(&self, code: StartupDegradedReasonCode) -> bool {
        self.degraded_reasons
            .iter()
            .any(|reason| reason.code == code)
    }

    pub fn push_reason(&mut self, code: StartupDegradedReasonCode, detail: impl Into<String>) {
        if !self.has_reason_code(code) {
            self.degraded_reasons
                .push(StartupDegradedReason::new(code, detail));
        }
    }

    pub fn is_degraded(&self) -> bool {
        !self.degraded_reasons.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vcpus: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u32>,
}

impl RunnerResourceLimits {
    fn validate(&self) -> Result<(), RunnerConfigError> {
        if let Some(max_vcpus) = self.max_vcpus
            && max_vcpus == 0
        {
            return Err(RunnerConfigError::InvalidResourceLimit {
                field: "max_vcpus",
                value: 0,
            });
        }
        if let Some(max_memory_mib) = self.max_memory_mib
            && max_memory_mib == 0
        {
            return Err(RunnerConfigError::InvalidResourceLimit {
                field: "max_memory_mib",
                value: 0,
            });
        }
        if let Some(max_processes) = self.max_processes
            && max_processes == 0
        {
            return Err(RunnerConfigError::InvalidResourceLimit {
                field: "max_processes",
                value: 0,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunnerBehaviorOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_tier: Option<SandboxTier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser_enabled: Option<bool>,
}

// ── Channel Configuration Types ─────────────────────────────────────────────

/// Per-user channel configuration. Lives in `RunnerUserConfig` (host-side,
/// per-user). Delivered to the VM via `RunnerBootstrapEnvelope`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramChannelConfig>,
    // Future: discord, whatsapp, etc.
}

impl ChannelsConfig {
    /// Returns `true` if no channel is configured.
    pub fn is_empty(&self) -> bool {
        self.telegram.is_none()
    }

    /// Collect all `bot_token_env` references from enabled channels.
    /// Returns environment variable names that the runner should forward
    /// to the VM process.
    pub fn bot_token_env_refs(&self) -> Vec<String> {
        let mut refs = Vec::new();
        if let Some(telegram) = &self.telegram
            && telegram.enabled
            && let Some(ref env_name) = telegram.bot_token_env
            && !env_name.is_empty()
        {
            refs.push(env_name.clone());
        }
        refs
    }
}

/// Telegram channel adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    /// Whether this channel is active. Defaults to `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Name of the environment variable holding the Telegram bot token.
    /// The runner forwards the resolved value to the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_env: Option<String>,
    /// Long-polling timeout in seconds. Defaults to 30.
    #[serde(default = "default_polling_timeout_secs")]
    pub polling_timeout_secs: u64,
    /// Authorized senders — only these platform IDs can interact with the agent.
    #[serde(default)]
    pub senders: Vec<SenderBinding>,
    /// Maximum Telegram message length (chars) before splitting. Defaults to 4096.
    #[serde(default = "default_max_message_length")]
    pub max_message_length: usize,
}

/// A sender identity binding: a set of platform-specific IDs that identify
/// the same person. All authorized senders are treated identically as the
/// owning user — there is no role differentiation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderBinding {
    /// Platform-specific sender identifiers (e.g. Telegram user_id strings).
    /// A single person may have multiple platform IDs.
    pub platform_ids: Vec<String>,
    /// Optional human-readable name for logging and audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

fn default_polling_timeout_secs() -> u64 {
    30
}

fn default_max_message_length() -> usize {
    4096
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum RunnerConfigError {
    #[error("runner workspace_root must not be empty")]
    InvalidWorkspaceRoot,
    #[error("runner user id must not be empty")]
    InvalidUserId,
    #[error("runner user `{user_id}` config_path must not be empty")]
    InvalidUserConfigPath { user_id: String },
    #[error("runner guest image reference `{image}` must not be empty")]
    InvalidGuestImageRef { image: &'static str },
    #[error("runner mount path `{mount}` must not be empty")]
    InvalidMountPath { mount: &'static str },
    #[error("runner resource limit `{field}` must be greater than zero; got {value}")]
    InvalidResourceLimit { field: &'static str, value: u64 },
    #[error("runner credential reference key must not be empty")]
    InvalidCredentialRefKey,
    #[error("runner credential reference value for key `{key}` must not be empty")]
    InvalidCredentialRefValue { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerBootstrapEnvelope {
    pub user_id: String,
    pub sandbox_tier: SandboxTier,
    pub workspace_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar_endpoint: Option<SidecarEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_policy: Option<RunnerRuntimePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_status: Option<StartupStatusReport>,
    /// Channel configuration (Telegram, etc.) for this user.
    /// Populated from the user's `RunnerUserConfig.channels`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channels: Option<ChannelsConfig>,
}

impl RunnerBootstrapEnvelope {
    pub fn validate(&self) -> Result<(), BootstrapEnvelopeError> {
        if self.user_id.trim().is_empty() {
            return Err(BootstrapEnvelopeError::InvalidField { field: "user_id" });
        }
        if self.workspace_root.trim().is_empty() {
            return Err(BootstrapEnvelopeError::InvalidField {
                field: "workspace_root",
            });
        }
        if let Some(sidecar) = &self.sidecar_endpoint
            && sidecar.address.trim().is_empty()
        {
            return Err(BootstrapEnvelopeError::InvalidField {
                field: "sidecar_endpoint.address",
            });
        }
        if let Some(runtime_policy) = &self.runtime_policy {
            runtime_policy.validate().map_err(|source| {
                BootstrapEnvelopeError::InvalidRuntimePolicy {
                    detail: source.to_string(),
                }
            })?;
        }
        if let Some(startup_status) = &self.startup_status {
            if startup_status.sandbox_tier != self.sandbox_tier {
                return Err(BootstrapEnvelopeError::InvalidField {
                    field: "startup_status.sandbox_tier",
                });
            }
            if self.sidecar_endpoint.is_none()
                && (startup_status.sidecar_available
                    || startup_status.shell_available
                    || startup_status.browser_available)
            {
                return Err(BootstrapEnvelopeError::InvalidField {
                    field: "startup_status.sidecar_available",
                });
            }
            if (startup_status.shell_available || startup_status.browser_available)
                && !startup_status.sidecar_available
            {
                return Err(BootstrapEnvelopeError::InvalidField {
                    field: "startup_status.shell_available",
                });
            }
        }
        Ok(())
    }

    pub fn to_length_prefixed_json(&self) -> Result<Vec<u8>, BootstrapEnvelopeError> {
        self.validate()?;

        let payload = serde_json::to_vec(self)?;
        if payload.len() > u32::MAX as usize {
            return Err(BootstrapEnvelopeError::EnvelopeTooLarge {
                payload_bytes: payload.len(),
            });
        }

        let mut frame = Vec::with_capacity(4 + payload.len());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(&payload);
        Ok(frame)
    }

    pub fn from_length_prefixed_json(frame: &[u8]) -> Result<Self, BootstrapEnvelopeError> {
        if frame.len() < 4 {
            return Err(BootstrapEnvelopeError::FrameTooShort {
                actual_bytes: frame.len(),
            });
        }

        let mut len_buf = [0_u8; 4];
        len_buf.copy_from_slice(&frame[..4]);
        let prefixed_len = u32::from_be_bytes(len_buf) as usize;
        let payload = &frame[4..];

        if payload.len() != prefixed_len {
            return Err(BootstrapEnvelopeError::LengthPrefixMismatch {
                prefixed_bytes: prefixed_len,
                payload_bytes: payload.len(),
            });
        }

        let envelope: Self = serde_json::from_slice(payload)?;
        envelope.validate()?;
        Ok(envelope)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarEndpoint {
    pub transport: SidecarTransport,
    pub address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidecarTransport {
    Unix,
    Vsock,
}

#[derive(Debug, Error)]
pub enum BootstrapEnvelopeError {
    #[error("bootstrap frame too short to contain length prefix: got {actual_bytes} bytes")]
    FrameTooShort { actual_bytes: usize },
    #[error(
        "bootstrap frame length prefix mismatch: prefixed={prefixed_bytes} payload={payload_bytes}"
    )]
    LengthPrefixMismatch {
        prefixed_bytes: usize,
        payload_bytes: usize,
    },
    #[error("bootstrap payload too large for u32 prefix: {payload_bytes} bytes")]
    EnvelopeTooLarge { payload_bytes: usize },
    #[error("bootstrap envelope field `{field}` is invalid")]
    InvalidField { field: &'static str },
    #[error("bootstrap runtime policy is invalid: {detail}")]
    InvalidRuntimePolicy { detail: String },
    #[error("bootstrap envelope serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "payload", rename_all = "snake_case")]
pub enum RunnerControl {
    HealthCheck,
    ShutdownUser { user_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerControlErrorCode {
    InvalidRequest,
    UnknownUser,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerControlError {
    pub code: RunnerControlErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerControlHealthStatus {
    pub user_id: String,
    pub healthy: bool,
    pub sandbox_tier: SandboxTier,
    pub startup_status: StartupStatusReport,
    pub shell_available: bool,
    pub browser_available: bool,
    pub shutdown: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_container_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerControlShutdownStatus {
    pub user_id: String,
    pub shutdown: bool,
    pub already_stopped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "payload", rename_all = "snake_case")]
pub enum RunnerControlResponse {
    HealthStatus(RunnerControlHealthStatus),
    ShutdownStatus(RunnerControlShutdownStatus),
    Error(RunnerControlError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "payload", rename_all = "snake_case")]
pub enum ShellDaemonRequest {
    SpawnSession(SpawnSession),
    ExecCommand(ExecCommand),
    StreamOutput(StreamOutput),
    KillSession(KillSession),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSession {
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommand {
    pub request_id: String,
    pub session_id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOutput {
    pub request_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSession {
    pub request_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", content = "payload", rename_all = "snake_case")]
pub enum ShellDaemonResponse {
    SpawnSession(SpawnSessionAck),
    ExecCommand(ExecCommandAck),
    StreamOutput(StreamOutputChunk),
    KillSession(KillSessionAck),
    Error(ShellDaemonError),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSessionAck {
    pub request_id: String,
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecCommandAck {
    pub request_id: String,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamOutputChunk {
    pub request_id: String,
    pub session_id: String,
    pub stream: ShellOutputStream,
    pub data: String,
    pub eof: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSessionAck {
    pub request_id: String,
    pub killed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellDaemonError {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub message: String,
}

fn validate_optional_path(
    mount: &'static str,
    path: Option<&str>,
) -> Result<(), RunnerConfigError> {
    if path.is_some_and(|value| value.trim().is_empty()) {
        return Err(RunnerConfigError::InvalidMountPath { mount });
    }
    Ok(())
}

fn validate_required_path(mount: &'static str, path: &str) -> Result<(), RunnerConfigError> {
    if path.trim().is_empty() {
        return Err(RunnerConfigError::InvalidMountPath { mount });
    }
    Ok(())
}

fn validate_credential_refs(
    credential_refs: &BTreeMap<String, String>,
) -> Result<(), RunnerConfigError> {
    for (key, value) in credential_refs {
        if key.trim().is_empty() {
            return Err(RunnerConfigError::InvalidCredentialRefKey);
        }
        if value.trim().is_empty() {
            return Err(RunnerConfigError::InvalidCredentialRefValue { key: key.clone() });
        }
    }
    Ok(())
}

fn default_runner_workspace_root() -> String {
    DEFAULT_RUNNER_WORKSPACE_ROOT.to_owned()
}

fn default_oxydra_vm_image() -> String {
    DEFAULT_OXYDRA_VM_IMAGE.to_owned()
}

fn default_shell_vm_image() -> String {
    DEFAULT_SHELL_VM_IMAGE.to_owned()
}
