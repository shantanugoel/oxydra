use super::*;

pub struct ToolRegistry {
    tools: BTreeMap<String, Box<dyn Tool>>,
    max_output_bytes: usize,
    security_policy: Option<Arc<dyn SecurityPolicy>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_OUTPUT_BYTES)
    }
}

impl ToolRegistry {
    pub fn new(max_output_bytes: usize) -> Self {
        Self {
            tools: BTreeMap::new(),
            max_output_bytes,
            security_policy: None,
        }
    }

    pub fn register<T>(&mut self, name: impl Into<String>, tool: T)
    where
        T: Tool + 'static,
    {
        self.tools.insert(name.into(), Box::new(tool));
    }

    pub fn register_core_tools(&mut self) {
        let wasm_runner = default_wasm_runner();
        register_runtime_tools(self, wasm_runner, BashTool::default());
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    pub fn schemas(&self) -> Vec<FunctionDecl> {
        self.tools.values().map(|tool| tool.schema()).collect()
    }

    pub fn set_security_policy(&mut self, policy: Arc<dyn SecurityPolicy>) {
        self.security_policy = Some(policy);
    }

    pub async fn execute(&self, name: &str, args: &str) -> Result<String, ToolError> {
        self.execute_with_policy(name, args, |_| Ok(())).await
    }

    pub async fn execute_with_policy<F>(
        &self,
        name: &str,
        args: &str,
        mut safety_gate: F,
    ) -> Result<String, ToolError>
    where
        F: FnMut(SafetyTier) -> Result<(), ToolError>,
    {
        let tool = self
            .get(name)
            .ok_or_else(|| execution_failed(name, format!("unknown tool `{name}`")))?;

        safety_gate(tool.safety_tier())?;
        if let Some(policy) = &self.security_policy {
            let arguments = parse_policy_args(name, args)?;
            policy
                .enforce(name, tool.safety_tier(), &arguments)
                .map_err(|violation| {
                    execution_failed(
                        name,
                        format!(
                            "blocked by security policy ({:?}): {}",
                            violation.reason, violation.detail
                        ),
                    )
                })?;
        }

        let timeout = tool.timeout();
        let output = tokio::time::timeout(timeout, tool.execute(args))
            .await
            .map_err(|_| execution_failed(name, format!("tool timed out after {timeout:?}")))??;

        Ok(truncate_output(output, self.max_output_bytes))
    }
}

pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register_core_tools();
    registry
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolAvailability {
    pub shell: SessionStatus,
    pub browser: SessionStatus,
}

pub struct RuntimeToolsBootstrap {
    pub registry: ToolRegistry,
    pub availability: ToolAvailability,
}

pub async fn bootstrap_runtime_tools(
    bootstrap: Option<&RunnerBootstrapEnvelope>,
) -> RuntimeToolsBootstrap {
    let (bash_tool, shell_status, browser_status) = bootstrap_bash_tool(bootstrap).await;
    let wasm_runner = runtime_wasm_runner(bootstrap);
    let mut registry = ToolRegistry::default();
    register_runtime_tools(&mut registry, wasm_runner, bash_tool);
    registry.set_security_policy(Arc::new(workspace_security_policy(bootstrap)));

    RuntimeToolsBootstrap {
        registry,
        availability: ToolAvailability {
            shell: shell_status,
            browser: browser_status,
        },
    }
}

fn register_runtime_tools(
    registry: &mut ToolRegistry,
    wasm_runner: Arc<dyn WasmToolRunner>,
    shell_tool: BashTool,
) {
    registry.register(FILE_READ_TOOL_NAME, ReadTool::new(wasm_runner.clone()));
    registry.register(FILE_SEARCH_TOOL_NAME, SearchTool::new(wasm_runner.clone()));
    registry.register(FILE_LIST_TOOL_NAME, ListTool::new(wasm_runner.clone()));
    registry.register(FILE_WRITE_TOOL_NAME, WriteTool::new(wasm_runner.clone()));
    registry.register(FILE_EDIT_TOOL_NAME, EditTool::new(wasm_runner.clone()));
    registry.register(FILE_DELETE_TOOL_NAME, DeleteTool::new(wasm_runner.clone()));
    registry.register(WEB_FETCH_TOOL_NAME, WebFetchTool::new(wasm_runner.clone()));
    registry.register(
        WEB_SEARCH_TOOL_NAME,
        WebSearchTool::new(wasm_runner.clone()),
    );
    registry.register(VAULT_COPYTO_TOOL_NAME, VaultCopyToTool::new(wasm_runner));
    registry.register(SHELL_EXEC_TOOL_NAME, shell_tool);
}
