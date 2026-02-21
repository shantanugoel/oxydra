use std::{
    env, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use memory::LibsqlMemory;
use provider::{
    AnthropicConfig, AnthropicProvider, OpenAIConfig, OpenAIProvider, ReliableProvider, RetryPolicy,
};
use runtime::{ContextBudgetLimits, RetrievalLimits, RuntimeLimits, SummarizationLimits};
use serde::Serialize;
use thiserror::Error;
use tools::{RuntimeToolsBootstrap, ToolAvailability, ToolRegistry, bootstrap_runtime_tools};
use types::{
    ANTHROPIC_PROVIDER_ID, AgentConfig, BootstrapEnvelopeError, ConfigError, Memory, MemoryError,
    OPENAI_PROVIDER_ID, Provider, ProviderError, RunnerBootstrapEnvelope,
};

const SYSTEM_CONFIG_DIR: &str = "/etc/oxydra";
const USER_CONFIG_DIR: &str = ".config/oxydra";
const WORKSPACE_CONFIG_DIR: &str = ".oxydra";
const AGENT_CONFIG_FILE: &str = "agent.toml";
const PROVIDERS_CONFIG_FILE: &str = "providers.toml";
const CONFIG_ENV_PREFIX: &str = "OXYDRA__";
const DEFAULT_PROFILE: &str = "default";

#[derive(Debug, Clone)]
pub struct ConfigSearchPaths {
    pub system_dir: PathBuf,
    pub user_dir: Option<PathBuf>,
    pub workspace_dir: PathBuf,
}

impl ConfigSearchPaths {
    pub fn discover() -> Result<Self, CliError> {
        let workspace_dir = env::current_dir()?.join(WORKSPACE_CONFIG_DIR);
        let user_dir = env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(USER_CONFIG_DIR));
        Ok(Self {
            system_dir: PathBuf::from(SYSTEM_CONFIG_DIR),
            user_dir,
            workspace_dir,
        })
    }
}

pub struct VmBootstrapRuntime {
    pub bootstrap: Option<RunnerBootstrapEnvelope>,
    pub config: AgentConfig,
    pub provider: Box<dyn Provider>,
    pub memory: Option<Arc<dyn Memory>>,
    pub runtime_limits: RuntimeLimits,
    pub tool_registry: ToolRegistry,
    pub tool_availability: ToolAvailability,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct CliOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory: Option<MemoryOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProviderOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reliability: Option<ReliabilityOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RuntimeOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_cost: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<ContextBudgetOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarization: Option<SummarizationOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MemoryOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub db_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retrieval: Option<RetrievalOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ContextBudgetOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safety_buffer_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_max_context_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SummarizationOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_turns: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RetrievalOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vector_weight: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fts_weight: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SelectionOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProviderOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAIProviderOverrides>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<AnthropicProviderOverrides>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OpenAIProviderOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AnthropicProviderOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReliabilityOverrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_base_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backoff_max_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jitter: Option<bool>,
}

#[derive(Debug, Error)]
pub enum CliError {
    #[error("failed to resolve configuration path: {0}")]
    Io(#[from] io::Error),
    #[error("failed to load configuration: {0}")]
    ConfigExtract(#[source] Box<figment::Error>),
    #[error(transparent)]
    ConfigValidation(#[from] ConfigError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Bootstrap(#[from] BootstrapEnvelopeError),
    #[error("unsupported provider `{provider}` in provider selection")]
    UnsupportedProvider { provider: String },
}

impl From<figment::Error> for CliError {
    fn from(value: figment::Error) -> Self {
        Self::ConfigExtract(Box::new(value))
    }
}

pub fn load_agent_config(
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<AgentConfig, CliError> {
    let paths = ConfigSearchPaths::discover()?;
    load_agent_config_with_paths(&paths, profile, cli_overrides)
}

pub fn load_agent_config_with_paths(
    paths: &ConfigSearchPaths,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<AgentConfig, CliError> {
    let selected_profile = profile
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PROFILE);

    let mut figment = Figment::from(Serialized::defaults(AgentConfig::default()));
    figment = merge_directory(figment, &paths.system_dir, selected_profile);
    if let Some(user_dir) = &paths.user_dir {
        figment = merge_directory(figment, user_dir, selected_profile);
    }
    figment = merge_directory(figment, &paths.workspace_dir, selected_profile);
    figment = figment.merge(Env::prefixed(CONFIG_ENV_PREFIX).split("__"));
    figment = figment.merge(Serialized::defaults(cli_overrides));

    let config: AgentConfig = figment.select(selected_profile).extract()?;
    config.validate()?;
    Ok(config)
}

pub fn build_reliable_provider(config: &AgentConfig) -> Result<ReliableProvider, CliError> {
    config.validate()?;

    let inner: Box<dyn Provider> = match config.selection.provider.0.as_str() {
        OPENAI_PROVIDER_ID => Box::new(OpenAIProvider::new(OpenAIConfig {
            api_key: config.providers.openai.api_key.clone(),
            base_url: config.providers.openai.base_url.clone(),
        })?),
        ANTHROPIC_PROVIDER_ID => Box::new(AnthropicProvider::new(AnthropicConfig {
            api_key: config.providers.anthropic.api_key.clone(),
            base_url: config.providers.anthropic.base_url.clone(),
            ..AnthropicConfig::default()
        })?),
        provider => {
            return Err(CliError::UnsupportedProvider {
                provider: provider.to_owned(),
            });
        }
    };

    inner.capabilities(&config.selection.model)?;

    Ok(ReliableProvider::new(
        inner,
        RetryPolicy {
            max_attempts: config.reliability.max_attempts,
            backoff_base: Duration::from_millis(config.reliability.backoff_base_ms),
            backoff_max: Duration::from_millis(config.reliability.backoff_max_ms),
        },
    ))
}

pub fn build_provider(config: &AgentConfig) -> Result<Box<dyn Provider>, CliError> {
    Ok(Box::new(build_reliable_provider(config)?))
}

pub async fn build_memory_backend(
    config: &AgentConfig,
) -> Result<Option<Arc<dyn Memory>>, CliError> {
    config.validate()?;
    let backend = LibsqlMemory::from_config(&config.memory).await?;
    Ok(backend.map(|memory| Arc::new(memory) as Arc<dyn Memory>))
}

pub fn runtime_limits(config: &AgentConfig) -> RuntimeLimits {
    RuntimeLimits {
        turn_timeout: Duration::from_secs(config.runtime.turn_timeout_secs),
        max_turns: config.runtime.max_turns,
        max_cost: config.runtime.max_cost,
        context_budget: ContextBudgetLimits {
            trigger_ratio: config.runtime.context_budget.trigger_ratio,
            safety_buffer_tokens: config.runtime.context_budget.safety_buffer_tokens,
            fallback_max_context_tokens: config.runtime.context_budget.fallback_max_context_tokens,
        },
        retrieval: RetrievalLimits {
            top_k: config.memory.retrieval.top_k,
            vector_weight: config.memory.retrieval.vector_weight,
            fts_weight: config.memory.retrieval.fts_weight,
        },
        summarization: SummarizationLimits {
            target_ratio: config.runtime.summarization.target_ratio,
            min_turns: config.runtime.summarization.min_turns,
        },
    }
}

pub async fn bootstrap_vm_runtime(
    bootstrap_frame: Option<&[u8]>,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<VmBootstrapRuntime, CliError> {
    let paths = ConfigSearchPaths::discover()?;
    bootstrap_vm_runtime_with_paths(&paths, bootstrap_frame, profile, cli_overrides).await
}

pub async fn bootstrap_vm_runtime_with_paths(
    paths: &ConfigSearchPaths,
    bootstrap_frame: Option<&[u8]>,
    profile: Option<&str>,
    cli_overrides: CliOverrides,
) -> Result<VmBootstrapRuntime, CliError> {
    let bootstrap = bootstrap_frame
        .map(RunnerBootstrapEnvelope::from_length_prefixed_json)
        .transpose()?;
    let config = load_agent_config_with_paths(paths, profile, cli_overrides)?;
    let provider = build_provider(&config)?;
    let memory = build_memory_backend(&config).await?;
    let runtime_limits = runtime_limits(&config);
    let RuntimeToolsBootstrap {
        registry,
        availability,
    } = bootstrap_runtime_tools(bootstrap.as_ref()).await;

    Ok(VmBootstrapRuntime {
        bootstrap,
        config,
        provider,
        memory,
        runtime_limits,
        tool_registry: registry,
        tool_availability: availability,
    })
}

fn merge_directory(mut figment: Figment, directory: &Path, selected_profile: &str) -> Figment {
    for file_name in [AGENT_CONFIG_FILE, PROVIDERS_CONFIG_FILE] {
        let path = directory.join(file_name);
        if path.is_file() {
            figment = if file_uses_profiles(&path, selected_profile) {
                figment.merge(Toml::file(path).nested())
            } else {
                figment.merge(Toml::file(path))
            };
        }
    }
    figment
}

fn file_uses_profiles(path: &Path, selected_profile: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&contents) else {
        return false;
    };
    let Some(table) = value.as_table() else {
        return false;
    };

    table.contains_key("default")
        || table.contains_key("global")
        || table.contains_key(selected_profile)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Mutex, OnceLock},
        time::{SystemTime, UNIX_EPOCH},
    };

    use tokio::sync::Mutex as AsyncMutex;
    use types::{ModelId, ProviderId, RunnerBootstrapEnvelope, SandboxTier};

    use super::*;

    fn test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn async_test_lock() -> &'static AsyncMutex<()> {
        static LOCK: OnceLock<AsyncMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| AsyncMutex::new(()))
    }

    #[derive(Debug)]
    struct EnvGuard {
        key: &'static str,
        previous: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
            unsafe { env::set_var(key, value) };
            Self { key, previous }
        }

        fn remove(key: &'static str) -> Self {
            let previous = env::var(key).ok();
            // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
            unsafe { env::remove_var(key) };
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.previous {
                // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
                unsafe { env::set_var(self.key, value) };
            } else {
                // SAFETY: tests hold a process-wide mutex while mutating env to avoid races.
                unsafe { env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn load_agent_config_honors_file_env_and_cli_precedence() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("precedence");
        let paths = test_paths(&root);
        write_config(
            &paths.system_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[runtime]
max_turns = 2
"#,
        );
        write_config(
            &paths.user_dir.clone().expect("user dir should be present"),
            AGENT_CONFIG_FILE,
            r#"
[runtime]
max_turns = 3
"#,
        );
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
[runtime]
max_turns = 4
"#,
        );
        write_config(
            &paths.workspace_dir,
            PROVIDERS_CONFIG_FILE,
            r#"
[providers.openai]
base_url = "https://workspace-openai.example"
"#,
        );

        let _clear_runtime = EnvGuard::remove("OXYDRA__RUNTIME__MAX_TURNS");
        let _clear_openai_base_url = EnvGuard::remove("OXYDRA__PROVIDERS__OPENAI__BASE_URL");
        let _runtime_override = EnvGuard::set("OXYDRA__RUNTIME__MAX_TURNS", "5");
        let _openai_base_url_override = EnvGuard::set(
            "OXYDRA__PROVIDERS__OPENAI__BASE_URL",
            "https://env-openai.example",
        );

        let config = load_agent_config_with_paths(
            &paths,
            None,
            CliOverrides {
                runtime: Some(RuntimeOverrides {
                    max_turns: Some(6),
                    ..RuntimeOverrides::default()
                }),
                ..CliOverrides::default()
            },
        )
        .expect("config should load");

        assert_eq!(config.runtime.max_turns, 6);
        assert_eq!(
            config.providers.openai.base_url,
            "https://env-openai.example"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_applies_profile_overrides() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("profile");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
[default]
config_version = "1.0.0"
[default.selection]
provider = "openai"
model = "gpt-4o-mini"
[default.runtime]
max_turns = 3
[prod.runtime]
max_turns = 11
"#,
        );

        let config = load_agent_config_with_paths(&paths, Some("prod"), CliOverrides::default())
            .expect("config should load");
        assert_eq!(config.runtime.max_turns, 11);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_maps_nested_env_keys() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("env-nesting");
        let paths = test_paths(&root);

        let _clear_selection_provider = EnvGuard::remove("OXYDRA__SELECTION__PROVIDER");
        let _clear_selection_model = EnvGuard::remove("OXYDRA__SELECTION__MODEL");
        let _clear_anthropic_base_url = EnvGuard::remove("OXYDRA__PROVIDERS__ANTHROPIC__BASE_URL");
        let _provider = EnvGuard::set("OXYDRA__SELECTION__PROVIDER", "anthropic");
        let _model = EnvGuard::set("OXYDRA__SELECTION__MODEL", "claude-3-5-haiku-latest");
        let _anthropic_base_url = EnvGuard::set(
            "OXYDRA__PROVIDERS__ANTHROPIC__BASE_URL",
            "https://anthropic-env.example",
        );

        let config = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect("config should load");

        assert_eq!(config.selection.provider, ProviderId::from("anthropic"));
        assert_eq!(
            config.providers.anthropic.base_url,
            "https://anthropic-env.example"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_rejects_unsupported_config_version() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("version");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "2.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
"#,
        );

        let error = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect_err("unsupported version should fail");
        assert!(matches!(
            error,
            CliError::ConfigValidation(ConfigError::UnsupportedConfigVersion { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn load_agent_config_rejects_remote_memory_without_auth_token() {
        let _lock = test_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = temp_dir("memory-remote-auth");
        let paths = test_paths(&root);
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[memory]
enabled = true
remote_url = "libsql://example-org.turso.io"
"#,
        );

        let error = load_agent_config_with_paths(&paths, None, CliOverrides::default())
            .expect_err("remote memory mode without auth token should fail validation");
        assert!(matches!(
            error,
            CliError::ConfigValidation(ConfigError::MissingMemoryAuthToken { .. })
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn provider_factory_switches_by_config_selection() {
        let mut openai_config = AgentConfig::default();
        openai_config.selection.provider = ProviderId::from("openai");
        openai_config.selection.model = ModelId::from("gpt-4o-mini");
        openai_config.providers.openai.api_key = Some("openai-test-key".to_owned());
        openai_config.reliability.max_attempts = 4;
        openai_config.reliability.backoff_base_ms = 10;
        openai_config.reliability.backoff_max_ms = 100;
        let reliable_openai =
            build_reliable_provider(&openai_config).expect("openai provider should be constructed");
        assert_eq!(reliable_openai.provider_id(), &ProviderId::from("openai"));
        assert_eq!(reliable_openai.retry_policy().max_attempts, 4);

        let mut anthropic_config = AgentConfig::default();
        anthropic_config.selection.provider = ProviderId::from("anthropic");
        anthropic_config.selection.model = ModelId::from("claude-3-5-haiku-latest");
        anthropic_config.providers.anthropic.api_key = Some("anthropic-test-key".to_owned());
        let provider = build_provider(&anthropic_config).expect("anthropic provider should build");
        assert_eq!(provider.provider_id(), &ProviderId::from("anthropic"));
    }

    #[tokio::test]
    async fn build_memory_backend_returns_none_when_memory_is_disabled() {
        let backend = build_memory_backend(&AgentConfig::default())
            .await
            .expect("disabled memory config should not fail");
        assert!(backend.is_none());
    }

    #[test]
    fn runtime_limits_maps_phase9_budget_retrieval_and_summarization_settings() {
        let mut config = AgentConfig::default();
        config.runtime.turn_timeout_secs = 30;
        config.runtime.max_turns = 5;
        config.runtime.max_cost = Some(3.25);
        config.runtime.context_budget.trigger_ratio = 0.9;
        config.runtime.context_budget.safety_buffer_tokens = 2_048;
        config.runtime.context_budget.fallback_max_context_tokens = 96_000;
        config.memory.retrieval.top_k = 12;
        config.memory.retrieval.vector_weight = 0.6;
        config.memory.retrieval.fts_weight = 0.4;
        config.runtime.summarization.target_ratio = 0.45;
        config.runtime.summarization.min_turns = 9;

        let limits = runtime_limits(&config);
        assert_eq!(limits.turn_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_turns, 5);
        assert_eq!(limits.max_cost, Some(3.25));
        assert_eq!(limits.context_budget.trigger_ratio, 0.9);
        assert_eq!(limits.context_budget.safety_buffer_tokens, 2_048);
        assert_eq!(limits.context_budget.fallback_max_context_tokens, 96_000);
        assert_eq!(limits.retrieval.top_k, 12);
        assert_eq!(limits.retrieval.vector_weight, 0.6);
        assert_eq!(limits.retrieval.fts_weight, 0.4);
        assert_eq!(limits.summarization.target_ratio, 0.45);
        assert_eq!(limits.summarization.min_turns, 9);
    }

    #[tokio::test]
    async fn bootstrap_vm_runtime_with_process_tier_frame_disables_sidecar_tools() {
        let _lock = async_test_lock().lock().await;
        let root = temp_dir("bootstrap-process-tier");
        let paths = test_paths(&root);
        write_bootstrap_config(&paths);
        let frame = RunnerBootstrapEnvelope {
            user_id: "alice".to_owned(),
            sandbox_tier: SandboxTier::Process,
            workspace_root: "/tmp/oxydra-alice".to_owned(),
            sidecar_endpoint: None,
        }
        .to_length_prefixed_json()
        .expect("process-tier bootstrap frame should encode");

        let bootstrap =
            bootstrap_vm_runtime_with_paths(&paths, Some(&frame), None, CliOverrides::default())
                .await
                .expect("bootstrap runtime should initialize");

        assert_eq!(
            bootstrap
                .bootstrap
                .as_ref()
                .map(|envelope| envelope.sandbox_tier),
            Some(SandboxTier::Process)
        );
        assert!(!bootstrap.tool_availability.shell.is_ready());
        assert!(!bootstrap.tool_availability.browser.is_ready());
        let error = bootstrap
            .tool_registry
            .execute("bash", r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("process-tier bootstrap should disable sidecar-dependent shell tool");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "bash" && message.contains("disabled")
        ));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn bootstrap_vm_runtime_without_frame_disables_sidecar_tools_for_direct_execution() {
        let _lock = async_test_lock().lock().await;
        let root = temp_dir("bootstrap-direct");
        let paths = test_paths(&root);
        write_bootstrap_config(&paths);

        let bootstrap =
            bootstrap_vm_runtime_with_paths(&paths, None, None, CliOverrides::default())
                .await
                .expect("direct bootstrap runtime should initialize");

        assert!(bootstrap.bootstrap.is_none());
        assert!(!bootstrap.tool_availability.shell.is_ready());
        assert!(!bootstrap.tool_availability.browser.is_ready());
        let error = bootstrap
            .tool_registry
            .execute("bash", r#"{"command":"printf should-not-run"}"#)
            .await
            .expect_err("direct runtime bootstrap should disable sidecar-dependent shell tool");
        assert!(matches!(
            error,
            types::ToolError::ExecutionFailed { tool, message }
                if tool == "bash" && message.contains("disabled")
        ));

        let _ = fs::remove_dir_all(root);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let mut path = env::temp_dir();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be monotonic")
            .as_nanos();
        path.push(format!(
            "oxydra-cli-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp dir should be creatable");
        path
    }

    fn test_paths(root: &Path) -> ConfigSearchPaths {
        ConfigSearchPaths {
            system_dir: root.join("system"),
            user_dir: Some(root.join("user")),
            workspace_dir: root.join("workspace"),
        }
    }

    fn write_config(dir: &Path, file_name: &str, content: &str) {
        fs::create_dir_all(dir).expect("config dir should be creatable");
        fs::write(dir.join(file_name), content.trim_start())
            .expect("config file should be writable");
    }

    fn write_bootstrap_config(paths: &ConfigSearchPaths) {
        write_config(
            &paths.workspace_dir,
            AGENT_CONFIG_FILE,
            r#"
config_version = "1.0.0"
[selection]
provider = "openai"
model = "gpt-4o-mini"
[providers.openai]
api_key = "test-openai-key"
"#,
        );
    }
}
