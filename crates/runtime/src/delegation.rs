use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use types::DelegationProgressSender;
use types::{
    AgentDefinition, DelegationRequest, DelegationResult, DelegationStatus, ProviderSelection,
    RuntimeError, ToolExecutionContext,
};
use types::{Context, Message, MessageRole};

use crate::AgentRuntime;

/// Simple runtime-backed delegation executor.
///
/// This implementation constructs a fresh context, injects the agent's
/// system prompt (if provided) and the delegation goal, then executes a
/// synchronous run of the parent's AgentRuntime for a dedicated subagent
/// session id. It reuses the parent's runtime with provider/model routes
/// selected per target agent (explicit override, caller inheritance, or root
/// fallback). For now tool allowlists are not enforced (all tools are
/// available to subagents). The implementation is intentionally simple but
/// functional for the common case.
pub struct RuntimeDelegationExecutor {
    runtime: Arc<AgentRuntime>,
    agents: BTreeMap<String, AgentDefinition>,
    root_selection: ProviderSelection,
}

impl RuntimeDelegationExecutor {
    pub fn new(
        runtime: Arc<AgentRuntime>,
        agents: BTreeMap<String, AgentDefinition>,
        root_selection: ProviderSelection,
    ) -> Self {
        Self {
            runtime,
            agents,
            root_selection,
        }
    }
}

fn resolve_delegation_selection(
    root_selection: &ProviderSelection,
    agent_name: &str,
    agent_def: &AgentDefinition,
    caller_selection: Option<&ProviderSelection>,
) -> ProviderSelection {
    if agent_name == "default" {
        return root_selection.clone();
    }
    if let Some(selection) = &agent_def.selection {
        return selection.clone();
    }
    caller_selection
        .cloned()
        .unwrap_or_else(|| root_selection.clone())
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
        // provider/model route based on agent-specific selection semantics.
        let effective_selection = resolve_delegation_selection(
            &self.root_selection,
            &request.agent_name,
            agent_def,
            request.caller_selection.as_ref(),
        );
        let mut ctx = Context {
            provider: effective_selection.provider,
            model: effective_selection.model,
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
        let tool_context = ToolExecutionContext {
            user_id: Some(request.parent_user_id.clone()),
            session_id: Some(subagent_session_id.clone()),
            provider: Some(ctx.provider.clone()),
            model: Some(ctx.model.clone()),
            channel_capabilities: None,
            event_sender: None,
            channel_id: None,
            channel_context_id: None,
            inbound_attachments: None,
        };
        let response = self
            .runtime
            .run_session_for_session_with_tool_context(
                &subagent_session_id,
                &mut ctx,
                parent_cancellation,
                &tool_context,
            )
            .await;

        match response {
            Ok(resp) => Ok(DelegationResult {
                output: resp.message.content.unwrap_or_default(),
                attachments: resp.message.attachments,
                turns_used: 1, // best-effort
                cost_used: 0.0,
                status: DelegationStatus::Completed,
            }),
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{ModelId, ProviderId};

    fn selection(provider: &str, model: &str) -> ProviderSelection {
        ProviderSelection {
            provider: ProviderId::from(provider),
            model: ModelId::from(model),
        }
    }

    #[test]
    fn delegation_selection_uses_agent_override_when_present() {
        let root = selection("openai", "gpt-4o-mini");
        let caller = selection("anthropic", "claude-3-5-haiku-latest");
        let agent = AgentDefinition {
            system_prompt: None,
            system_prompt_file: None,
            selection: Some(selection("gemini", "gemini-2.5-pro")),
            tools: None,
            max_turns: None,
            max_cost: None,
        };

        let effective = resolve_delegation_selection(&root, "researcher", &agent, Some(&caller));
        assert_eq!(effective, selection("gemini", "gemini-2.5-pro"));
    }

    #[test]
    fn delegation_selection_inherits_caller_when_agent_has_no_override() {
        let root = selection("openai", "gpt-4o-mini");
        let caller = selection("anthropic", "claude-3-5-haiku-latest");
        let agent = AgentDefinition {
            system_prompt: None,
            system_prompt_file: None,
            selection: None,
            tools: None,
            max_turns: None,
            max_cost: None,
        };

        let effective = resolve_delegation_selection(&root, "coder", &agent, Some(&caller));
        assert_eq!(effective, caller);
    }

    #[test]
    fn delegation_selection_for_default_agent_always_uses_root() {
        let root = selection("openai", "gpt-4o-mini");
        let caller = selection("anthropic", "claude-3-5-haiku-latest");
        let agent = AgentDefinition {
            system_prompt: None,
            system_prompt_file: None,
            selection: Some(selection("gemini", "gemini-2.5-pro")),
            tools: None,
            max_turns: None,
            max_cost: None,
        };

        let effective = resolve_delegation_selection(&root, "default", &agent, Some(&caller));
        assert_eq!(effective, root);
    }
}
