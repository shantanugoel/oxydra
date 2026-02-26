use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    FunctionDecl, SafetyTier, Tool, ToolError, ToolExecutionContext, execution_failed, parse_args,
};
use types::DelegationRequest;

pub const DELEGATE_TO_AGENT_TOOL_NAME: &str = "delegate_to_agent";

#[derive(Debug, Deserialize)]
struct DelegateArgs {
    agent_name: String,
    goal: String,
    key_facts: Option<Vec<String>>,
    max_turns: Option<u32>,
    max_cost: Option<f64>,
}

pub struct DelegateToAgentTool;

impl DelegateToAgentTool {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl Tool for DelegateToAgentTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            DELEGATE_TO_AGENT_TOOL_NAME,
            Some(
                "Delegate the given goal to a named specialist agent. Returns the agent's output."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["agent_name", "goal"],
                "properties": {
                    "agent_name": { "type": "string", "description": "Name of the specialist agent to delegate to" },
                    "goal": { "type": "string", "description": "What the subagent should accomplish" },
                    "key_facts": { "type": "array", "items": { "type": "string" }, "description": "Optional key facts to prime the subagent" },
                    "max_turns": { "type": "integer", "minimum": 1, "description": "Optional max turns for the subagent" },
                    "max_cost": { "type": "number", "description": "Optional max cost for the subagent" }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: DelegateArgs = parse_args(DELEGATE_TO_AGENT_TOOL_NAME, args)?;

        let user_id = context.user_id.clone().ok_or_else(|| {
            execution_failed(DELEGATE_TO_AGENT_TOOL_NAME, "user context not available")
        })?;

        let executor = types::get_global_delegation_executor().ok_or_else(|| {
            execution_failed(
                DELEGATE_TO_AGENT_TOOL_NAME,
                "delegation executor not available",
            )
        })?;

        let parent_session_id = context.session_id.clone().unwrap_or_else(|| "".to_owned());

        let del_req = DelegationRequest {
            parent_session_id,
            parent_user_id: user_id,
            agent_name: request.agent_name,
            goal: request.goal,
            key_facts: request.key_facts.unwrap_or_default(),
            max_turns: request.max_turns,
            max_cost: request.max_cost,
        };

        // We cannot obtain the runtime cancellation token here; create a fresh one.
        let cancellation = CancellationToken::new();

        let result = executor
            .delegate(del_req, &cancellation, None)
            .await
            .map_err(|e| {
                execution_failed(
                    DELEGATE_TO_AGENT_TOOL_NAME,
                    format!("delegation failed: {e}"),
                )
            })?;

        // Return the subagent output as plain text. Caller can parse the JSON
        // if they require metadata; otherwise plain string is most convenient.
        Ok(result.output)
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

pub fn register_delegation_tools(registry: &mut crate::ToolRegistry) {
    registry.register(DELEGATE_TO_AGENT_TOOL_NAME, DelegateToAgentTool::new());
}
