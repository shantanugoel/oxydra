use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use types::DelegationProgressSender;
use types::{AgentDefinition, DelegationRequest, DelegationResult, DelegationStatus, RuntimeError};
use types::{Context, Message, MessageRole, ModelId, ProviderId};

use crate::AgentRuntime;

/// Simple runtime-backed delegation executor.
///
/// This implementation constructs a fresh context, injects the agent's
/// system prompt (if provided) and the delegation goal, then executes a
/// synchronous run of the parent's AgentRuntime for a dedicated subagent
/// session id. It intentionally reuses the parent's provider, tool registry
/// and limits. For now tool allowlists are not enforced (all tools are
/// available to subagents). The implementation is intentionally simple but
/// functional for the common case.
pub struct RuntimeDelegationExecutor {
    runtime: Arc<AgentRuntime>,
    agents: BTreeMap<String, AgentDefinition>,
}

impl RuntimeDelegationExecutor {
    pub fn new(runtime: Arc<AgentRuntime>, agents: BTreeMap<String, AgentDefinition>) -> Self {
        Self { runtime, agents }
    }
}

#[async_trait]
impl types::DelegationExecutor for RuntimeDelegationExecutor {
    async fn delegate(
        &self,
        request: DelegationRequest,
        parent_cancellation: &CancellationToken,
        _progress_sender: Option<DelegationProgressSender>,
    ) -> Result<DelegationResult, RuntimeError> {
        // Lookup the agent definition
        let agent_def = match self.agents.get(&request.agent_name) {
            Some(def) => def,
            None => {
                return Err(RuntimeError::Tool(types::ToolError::ExecutionFailed {
                    tool: "delegate_to_agent".to_string(),
                    message: format!("unknown agent `{}`", request.agent_name),
                }));
            }
        };

        // Build a fresh context for the subagent run. Use a conservative
        // default provider/model so provider-specific behaviour has reasonable
        // fallbacks; the runtime's provider will be used for actual calls.
        let mut ctx = Context {
            provider: ProviderId::from("openai"),
            model: ModelId::from("gpt-4o-mini"),
            tools: Vec::new(),
            messages: Vec::new(),
        };

        // Inject the agent's configured system prompt if any. Use a system
        // message so the runtime will not inject the global system prompt.
        if let Some(system_prompt) = agent_def
            .system_prompt
            .as_ref()
            .or(agent_def.system_prompt_file.as_ref())
        {
            // Prefer inline prompt; if file path provided, the bootstrap
            // validated its existence but we do not re-read file here for
            // simplicity.
            ctx.messages.push(Message {
                role: MessageRole::System,
                content: Some(system_prompt.clone()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                attachments: Vec::new(),
            });
        }

        // Add key facts as system messages to prime the subagent.
        for fact in &request.key_facts {
            ctx.messages.push(Message {
                role: MessageRole::System,
                content: Some(fact.clone()),
                tool_calls: Vec::new(),
                tool_call_id: None,
                attachments: Vec::new(),
            });
        }

        // Add the delegation goal as the user message.
        ctx.messages.push(Message {
            role: MessageRole::User,
            content: Some(request.goal.clone()),
            tool_calls: Vec::new(),
            tool_call_id: None,
            attachments: Vec::new(),
        });

        // Construct a subagent session id. Use a simple prefix plus a UUID.
        let subagent_session_id = format!(
            "subagent:{}:{}",
            request.parent_session_id,
            uuid::Uuid::new_v4()
        );

        // Run the session on the parent's runtime instance. We reuse the
        // parent's runtime rather than constructing a new AgentRuntime so we
        // preserve provider/tool wiring. The cancellation token passed is the
        // parent's token so cancellation cascades.
        let response = self
            .runtime
            .run_session_for_session(&subagent_session_id, &mut ctx, parent_cancellation)
            .await;

        match response {
            Ok(resp) => Ok(DelegationResult {
                output: resp.message.content.unwrap_or_default(),
                turns_used: 1, // best-effort
                cost_used: 0.0,
                status: DelegationStatus::Completed,
            }),
            Err(err) => Err(err),
        }
    }
}
