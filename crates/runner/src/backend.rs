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
