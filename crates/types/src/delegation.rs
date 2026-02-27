use async_trait::async_trait;
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize};
use std::{fmt, sync::Arc};
use tokio_util::sync::CancellationToken;

use crate::{InlineMedia, ProviderSelection, RuntimeError, StreamItem};

/// Progress reporter used by delegation implementations. Implementations can
/// provide a closure that accepts StreamItem values and forwards them to the
/// runtime's event stream (e.g., an mpsc::UnboundedSender<StreamItem> wrapped
/// in a closure).
pub type DelegationProgressSender = Box<dyn Fn(StreamItem) + Send + Sync + 'static>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationRequest {
    pub parent_session_id: String,
    pub parent_user_id: String,
    pub agent_name: String,
    pub goal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_selection: Option<ProviderSelection>,
    #[serde(default)]
    pub key_facts: Vec<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_cost: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelegationResult {
    pub output: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<InlineMedia>,
    pub turns_used: usize,
    pub cost_used: f64,
    pub status: DelegationStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DelegationStatus {
    Completed,
    BudgetExhausted { partial_output: String },
    Failed { error: String },
}

#[async_trait]
pub trait DelegationExecutor: Send + Sync {
    async fn delegate(
        &self,
        request: DelegationRequest,
        parent_cancellation: &CancellationToken,
        progress_sender: Option<DelegationProgressSender>,
    ) -> Result<DelegationResult, RuntimeError>;
}

// Global holder for a runtime-provided delegation executor. The tool registry
// registers a delegation tool at bootstrap which will look up this global
// executor when invoked. The VM runtime sets the global executor during
// startup once the AgentRuntime has been constructed.
static GLOBAL_DELEGATION_EXECUTOR: OnceCell<Arc<dyn DelegationExecutor>> = OnceCell::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetGlobalDelegationExecutorError;

impl fmt::Display for SetGlobalDelegationExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("delegation executor already initialized")
    }
}

impl std::error::Error for SetGlobalDelegationExecutorError {}

/// Set the global delegation executor. Returns Err if already set.
pub fn set_global_delegation_executor(
    ex: Arc<dyn DelegationExecutor>,
) -> Result<(), SetGlobalDelegationExecutorError> {
    GLOBAL_DELEGATION_EXECUTOR
        .set(ex)
        .map_err(|_| SetGlobalDelegationExecutorError)
}

/// Get a clone of the global delegation executor if present.
pub fn get_global_delegation_executor() -> Option<Arc<dyn DelegationExecutor>> {
    GLOBAL_DELEGATION_EXECUTOR.get().cloned()
}
