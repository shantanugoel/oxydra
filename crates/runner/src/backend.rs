use super::*;

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
        let mut degraded_reasons = Vec::new();
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
        let runtime = self
            .spawn_process_guest_with_startup_stdin(RunnerGuestRole::OxydraVm, runtime_command)?;
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

        run_async(launch_docker_container_async(
            endpoint.clone(),
            container_name.clone(),
            image.to_owned(),
            request.workspace.clone(),
            request.mounts.clone(),
            request.resource_limits.clone(),
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

    fn spawn_process_guest_with_startup_stdin(
        &self,
        role: RunnerGuestRole,
        command: RunnerCommandSpec,
    ) -> Result<RunnerGuestHandle, RunnerError> {
        let mut child_command = Command::new(&command.program);
        child_command
            .args(&command.args)
            .stdin(Stdio::piped())
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
