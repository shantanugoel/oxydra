use std::path::Path;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use types::{BootstrapEnvelopeError, RunnerBootstrapEnvelope};

use super::*;

/// Maximum size (in bytes) of the base64-encoded bootstrap payload that can be
/// injected into the kernel command line. The Linux kernel typically allows
/// ~4096 bytes total for `boot_args`; we reserve headroom for existing args.
pub(crate) const FIRECRACKER_BOOTSTRAP_CMDLINE_MAX_BYTES: usize = 2048;

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
            SandboxTier::Process => self.launch_process(&request),
        }
    }
}

impl CrateSandboxBackend {
    fn launch_microvm_linux(
        &self,
        request: &SandboxLaunchRequest,
    ) -> Result<SandboxLaunch, RunnerError> {
        let fc_oxydra_config = request
            .guest_images
            .firecracker_oxydra_vm_config
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .ok_or(RunnerError::MissingFirecrackerConfig {
                field: "firecracker_oxydra_vm_config",
            })?;

        // Generate a per-launch config with bootstrap injected into boot_args.
        let generated_config = request.workspace.tmp.join("firecracker-oxydra-vm.json");
        generate_firecracker_config(
            fc_oxydra_config,
            request.bootstrap_file.as_deref(),
            &generated_config,
        )?;

        let runtime_api_socket = request.workspace.tmp.join("oxydra-vm-firecracker.sock");
        let runtime_command = RunnerCommandSpec::new(
            FIRECRACKER_BINARY,
            vec![
                "--api-sock".to_owned(),
                runtime_api_socket.to_string_lossy().into_owned(),
                "--config-file".to_owned(),
                generated_config.to_string_lossy().into_owned(),
            ],
        );
        let mut runtime = self.spawn_process_guest(
            RunnerGuestRole::OxydraVm,
            runtime_command,
            Some(&request.workspace.logs),
        )?;
        if let Err(error) = ensure_firecracker_api_ready(runtime_api_socket.clone()) {
            let _ = runtime.shutdown();
            return Err(error);
        }

        let mut warnings = Vec::new();
        let mut degraded_reasons = Vec::new();
        let mut sidecar = None;
        if request.sidecar_requested() {
            let fc_shell_config = match request
                .guest_images
                .firecracker_shell_vm_config
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                Some(path) => path.to_owned(),
                None => {
                    let detail =
                        "linux microvm sidecar launch failed: missing firecracker_shell_vm_config"
                            .to_owned();
                    warnings.push(detail.clone());
                    degraded_reasons.push(StartupDegradedReason::new(
                        StartupDegradedReasonCode::SidecarUnavailable,
                        detail,
                    ));
                    return Ok(SandboxLaunch {
                        launch: RunnerLaunchHandle {
                            tier: SandboxTier::MicroVm,
                            runtime,
                            sidecar: None,
                            scope: None,
                        },
                        sidecar_endpoint: None,
                        shell_available: false,
                        browser_available: false,
                        degraded_reasons,
                        warnings,
                    });
                }
            };

            // Generate a per-launch sidecar config (bootstrap not injected for sidecar).
            let generated_sidecar_config = request.workspace.tmp.join("firecracker-shell-vm.json");
            if let Err(error) = generate_firecracker_config(
                &fc_shell_config,
                None, // sidecar does not need bootstrap
                &generated_sidecar_config,
            ) {
                let detail = format!("linux microvm sidecar config generation failed: {error}");
                warnings.push(detail.clone());
                degraded_reasons.push(StartupDegradedReason::new(
                    StartupDegradedReasonCode::SidecarUnavailable,
                    detail,
                ));
                return Ok(SandboxLaunch {
                    launch: RunnerLaunchHandle {
                        tier: SandboxTier::MicroVm,
                        runtime,
                        sidecar: None,
                        scope: None,
                    },
                    sidecar_endpoint: None,
                    shell_available: false,
                    browser_available: false,
                    degraded_reasons,
                    warnings,
                });
            }

            let sidecar_api_socket = request.workspace.tmp.join("shell-vm-firecracker.sock");
            let sidecar_command = RunnerCommandSpec::new(
                FIRECRACKER_BINARY,
                vec![
                    "--api-sock".to_owned(),
                    sidecar_api_socket.to_string_lossy().into_owned(),
                    "--config-file".to_owned(),
                    generated_sidecar_config.to_string_lossy().into_owned(),
                ],
            );
            match self.spawn_process_guest(
                RunnerGuestRole::ShellVm,
                sidecar_command,
                Some(&request.workspace.logs),
            ) {
                Ok(mut handle) => {
                    if let Err(error) = ensure_firecracker_api_ready(sidecar_api_socket) {
                        let _ = handle.shutdown();
                        let detail = format!("linux microvm sidecar launch failed: {error}");
                        warnings.push(detail.clone());
                        degraded_reasons.push(StartupDegradedReason::new(
                            StartupDegradedReasonCode::SidecarUnavailable,
                            detail,
                        ));
                    } else {
                        sidecar = Some(handle);
                    }
                }
                Err(error) => {
                    let detail = format!("linux microvm sidecar launch failed: {error}");
                    warnings.push(detail.clone());
                    degraded_reasons.push(StartupDegradedReason::new(
                        StartupDegradedReasonCode::SidecarUnavailable,
                        detail,
                    ));
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
            degraded_reasons,
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
        let mut degraded_reasons = Vec::new();
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
                    let detail = format!("macOS microvm sidecar launch failed: {error}");
                    warnings.push(detail.clone());
                    degraded_reasons.push(StartupDegradedReason::new(
                        StartupDegradedReasonCode::SidecarUnavailable,
                        detail,
                    ));
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
            degraded_reasons,
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
        let mut degraded_reasons = Vec::new();
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
                Err(error) => {
                    let detail = format!("container sidecar launch failed: {error}");
                    warnings.push(detail.clone());
                    degraded_reasons.push(StartupDegradedReason::new(
                        StartupDegradedReasonCode::SidecarUnavailable,
                        detail,
                    ));
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
                tier: SandboxTier::Container,
                runtime,
                sidecar,
                scope: None,
            },
            sidecar_endpoint,
            shell_available,
            browser_available,
            degraded_reasons,
            warnings,
        })
    }

    fn launch_process(&self, request: &SandboxLaunchRequest) -> Result<SandboxLaunch, RunnerError> {
        let runtime_command = RunnerCommandSpec::new(
            resolve_process_tier_executable(),
            vec![
                "--user-id".to_owned(),
                request.user_id.clone(),
                "--workspace-root".to_owned(),
                request.workspace.root.to_string_lossy().into_owned(),
                "--bootstrap-stdin".to_owned(),
            ],
        );
        let runtime = self.spawn_process_guest_with_startup_stdin(
            RunnerGuestRole::OxydraVm,
            runtime_command,
            Some(&request.workspace.logs),
        )?;
        let hardening_attempt = attempt_process_tier_hardening();

        let mut warnings = vec![PROCESS_TIER_WARNING.to_owned()];
        let mut degraded_reasons = vec![StartupDegradedReason::new(
            StartupDegradedReasonCode::InsecureProcessTier,
            PROCESS_TIER_WARNING,
        )];
        if hardening_attempt.outcome != ProcessHardeningOutcome::Success {
            let detail = format!(
                "process-tier hardening {:?}: {}",
                hardening_attempt.outcome, hardening_attempt.detail
            );
            warnings.push(detail.clone());
            degraded_reasons.push(StartupDegradedReason::new(
                StartupDegradedReasonCode::ProcessHardeningLimited,
                detail,
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
            degraded_reasons,
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

        run_async(launch_docker_container_async(DockerContainerLaunchParams {
            endpoint: endpoint.clone(),
            container_name: container_name.clone(),
            image: image.to_owned(),
            workspace: request.workspace.clone(),
            mounts: request.mounts.clone(),
            resource_limits: request.resource_limits.clone(),
            labels,
            bootstrap_file: request.bootstrap_file.clone(),
        }))?;

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
        log_dir: Option<&Path>,
    ) -> Result<RunnerGuestHandle, RunnerError> {
        let mut child_command = Command::new(&command.program);
        child_command
            .args(&command.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = child_command
            .spawn()
            .map_err(|source| RunnerError::LaunchGuest {
                role,
                program: command.program.clone(),
                source,
            })?;
        let log_tasks = if let Some(log_dir) = log_dir {
            spawn_log_pump_threads(&mut child, log_dir, role)
        } else {
            Vec::new()
        };
        let mut handle = RunnerGuestHandle::from_child(role, command, child);
        handle.log_tasks = log_tasks;
        Ok(handle)
    }

    fn spawn_process_guest_with_startup_stdin(
        &self,
        role: RunnerGuestRole,
        command: RunnerCommandSpec,
        log_dir: Option<&Path>,
    ) -> Result<RunnerGuestHandle, RunnerError> {
        let mut child_command = Command::new(&command.program);
        child_command
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = child_command
            .spawn()
            .map_err(|source| RunnerError::LaunchGuest {
                role,
                program: command.program.clone(),
                source,
            })?;
        let log_tasks = if let Some(log_dir) = log_dir {
            spawn_log_pump_threads(&mut child, log_dir, role)
        } else {
            Vec::new()
        };
        let mut handle = RunnerGuestHandle::from_child(role, command, child);
        handle.log_tasks = log_tasks;
        Ok(handle)
    }
}

fn resolve_process_tier_executable() -> String {
    let explicit = std::env::var(PROCESS_EXECUTABLE_ENV_KEY)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if let Some(explicit) = explicit {
        return explicit;
    }

    if let Some(bundled) = bundled_process_tier_executable() {
        return bundled;
    }

    DEFAULT_PROCESS_EXECUTABLE.to_owned()
}

fn bundled_process_tier_executable() -> Option<String> {
    let current_executable = std::env::current_exe().ok()?;
    let parent = current_executable.parent()?;
    let bundled = parent.join(DEFAULT_PROCESS_EXECUTABLE);
    bundled
        .is_file()
        .then(|| bundled.to_string_lossy().into_owned())
}

/// Reads a Firecracker JSON config template and optionally injects the
/// bootstrap envelope as a base64-encoded kernel cmdline parameter
/// (`oxydra.bootstrap_b64=<b64>`). Writes the generated config to
/// `output_path`.
///
/// If `bootstrap_file` is `None`, the template is copied as-is (but still
/// validated as JSON).
pub(crate) fn generate_firecracker_config(
    template_path: &str,
    bootstrap_file: Option<&Path>,
    output_path: &Path,
) -> Result<(), RunnerError> {
    let template_content =
        fs::read_to_string(template_path).map_err(|source| RunnerError::ReadConfig {
            path: PathBuf::from(template_path),
            source,
        })?;
    let mut config: serde_json::Value =
        serde_json::from_str(&template_content).map_err(|source| {
            RunnerError::ParseFirecrackerConfig {
                path: PathBuf::from(template_path),
                source,
            }
        })?;

    if let Some(bootstrap_path) = bootstrap_file {
        let bootstrap_bytes =
            fs::read(bootstrap_path).map_err(|source| RunnerError::ReadConfig {
                path: bootstrap_path.to_path_buf(),
                source,
            })?;
        let bootstrap: RunnerBootstrapEnvelope =
            serde_json::from_slice(&bootstrap_bytes).map_err(BootstrapEnvelopeError::from)?;
        let bootstrap_json =
            serde_json::to_string(&bootstrap).map_err(BootstrapEnvelopeError::from)?;
        let bootstrap_b64 = BASE64_STANDARD.encode(bootstrap_json.as_bytes());

        if bootstrap_b64.len() > FIRECRACKER_BOOTSTRAP_CMDLINE_MAX_BYTES {
            return Err(RunnerError::BootstrapTooLargeForCmdline {
                bytes: bootstrap_b64.len(),
                max_bytes: FIRECRACKER_BOOTSTRAP_CMDLINE_MAX_BYTES,
            });
        }

        if let Some(boot_source) = config.get_mut("boot-source") {
            if let Some(boot_args) = boot_source.get_mut("boot_args") {
                let existing = boot_args.as_str().unwrap_or("");
                *boot_args = serde_json::Value::String(format!(
                    "{existing} oxydra.bootstrap_b64={bootstrap_b64}"
                ));
            } else {
                boot_source["boot_args"] =
                    serde_json::Value::String(format!("oxydra.bootstrap_b64={bootstrap_b64}"));
            }
        } else {
            config["boot-source"] = serde_json::json!({
                "boot_args": format!("oxydra.bootstrap_b64={bootstrap_b64}")
            });
        }
    }

    let output = serde_json::to_string_pretty(&config).map_err(BootstrapEnvelopeError::from)?;
    fs::write(output_path, output).map_err(|source| RunnerError::ProvisionWorkspace {
        path: output_path.to_path_buf(),
        source,
    })?;

    Ok(())
}
