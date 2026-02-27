use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    FunctionDecl, SafetyTier, Tool, ToolError, ToolExecutionContext, execution_failed, parse_args,
};
use types::{
    AgentDefinition, DelegationRequest, MediaAttachment, MediaType, ProviderSelection, StreamItem,
};

pub const DELEGATE_TO_AGENT_TOOL_NAME: &str = "delegate_to_agent";

#[derive(Debug, Deserialize)]
struct DelegateArgs {
    agent_name: String,
    goal: String,
    key_facts: Option<Vec<String>>,
    max_turns: Option<u32>,
    max_cost: Option<f64>,
}

pub struct DelegateToAgentTool {
    schema: FunctionDecl,
}

impl DelegateToAgentTool {
    pub fn new(agents: &BTreeMap<String, AgentDefinition>) -> Self {
        let mut supported_agents = Vec::new();
        let mut agent_descriptions = Vec::new();
        for (name, definition) in agents {
            supported_agents.push(name.clone());
            let description = definition
                .system_prompt
                .as_deref()
                .and_then(|prompt| prompt.lines().find(|line| !line.trim().is_empty()))
                .map(|line| line.trim().to_owned())
                .or_else(|| {
                    definition
                        .system_prompt_file
                        .as_ref()
                        .map(|path| format!("uses prompt file `{path}`"))
                })
                .unwrap_or_else(|| "specialist agent".to_owned());
            agent_descriptions.push(format!("{name}: {description}"));
        }
        let agent_name_schema = if supported_agents.is_empty() {
            json!({
                "type": "string",
                "description": "Name of the specialist agent to delegate to"
            })
        } else {
            json!({
                "type": "string",
                "enum": supported_agents,
                "description": format!(
                    "Name of the specialist agent to delegate to. Available: {}",
                    agent_descriptions.join("; ")
                )
            })
        };

        let schema = FunctionDecl::new(
            DELEGATE_TO_AGENT_TOOL_NAME,
            Some(
                "Delegate the given goal to a named specialist agent. Returns the agent's output."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["agent_name", "goal"],
                "properties": {
                    "agent_name": agent_name_schema,
                    "goal": { "type": "string", "description": "What the subagent should accomplish" },
                    "key_facts": { "type": "array", "items": { "type": "string" }, "description": "Optional key facts to prime the subagent" },
                    "max_turns": { "type": "integer", "minimum": 1, "description": "Optional max turns for the subagent" },
                    "max_cost": { "type": "number", "description": "Optional max cost for the subagent" }
                }
            }),
        );

        Self { schema }
    }
}

fn media_type_from_mime(mime_type: &str) -> MediaType {
    if mime_type.starts_with("image/") {
        MediaType::Photo
    } else if mime_type.starts_with("audio/") {
        MediaType::Audio
    } else if mime_type.starts_with("video/") {
        MediaType::Video
    } else {
        MediaType::Document
    }
}

fn extension_from_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "audio/mpeg" => "mp3",
        "audio/ogg" => "ogg",
        "video/mp4" => "mp4",
        _ => "bin",
    }
}

#[async_trait]
impl Tool for DelegateToAgentTool {
    fn schema(&self) -> FunctionDecl {
        self.schema.clone()
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
            caller_selection: match (&context.provider, &context.model) {
                (Some(provider), Some(model)) => Some(ProviderSelection {
                    provider: provider.clone(),
                    model: model.clone(),
                }),
                _ => None,
            },
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

        if let Some(sender) = context.event_sender.as_ref() {
            for (index, attachment) in result.attachments.iter().enumerate() {
                let extension = extension_from_mime(&attachment.mime_type);
                let file_name = format!("delegated-output-{}.{}", index + 1, extension);
                let file_path = format!("/tmp/{file_name}");
                let _ = sender.send(StreamItem::Media(MediaAttachment {
                    file_path,
                    media_type: media_type_from_mime(&attachment.mime_type),
                    caption: None,
                    data: attachment.data.clone(),
                    file_name: Some(file_name),
                }));
            }
        }

        if !result.attachments.is_empty() {
            let notice = format!(
                "Delegated agent completed with {} attachment(s), and they have already been delivered to the user. Do not call send_media for these delegated outputs, and do not re-delegate the same goal unless the user asks for more variants.",
                result.attachments.len()
            );
            if result.output.trim().is_empty() {
                return Ok(notice);
            }
            return Ok(format!("{notice}\n\nSubagent summary:\n{}", result.output));
        }

        Ok(result.output)
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(120)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

pub fn register_delegation_tools(
    registry: &mut crate::ToolRegistry,
    agents: &BTreeMap<String, AgentDefinition>,
) {
    registry.register(
        DELEGATE_TO_AGENT_TOOL_NAME,
        DelegateToAgentTool::new(agents),
    );
}
