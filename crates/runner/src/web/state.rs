use std::path::PathBuf;

use types::RunnerGlobalConfig;

/// Shared state for the web configurator server.
#[derive(Debug, Clone)]
pub struct WebState {
    /// The parsed global config (used for user registry, workspace root, etc.).
    pub global_config: RunnerGlobalConfig,
    /// Absolute path to the runner global config file.
    pub config_path: PathBuf,
    /// Resolved workspace root directory (absolute).
    pub workspace_root: PathBuf,
}

impl WebState {
    pub fn new(global_config: RunnerGlobalConfig, config_path: PathBuf) -> Self {
        let config_dir = config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let configured_root = PathBuf::from(&global_config.workspace_root);
        let workspace_root = if configured_root.is_absolute() {
            configured_root
        } else {
            config_dir.join(configured_root)
        };
        Self {
            global_config,
            config_path,
            workspace_root,
        }
    }

    /// Directory containing the runner config file (used to resolve relative paths).
    pub fn config_dir(&self) -> PathBuf {
        self.config_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }
}
