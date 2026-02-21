use super::{scrubbing::scrub_tool_output, *};

impl AgentRuntime {
    pub(crate) fn invalid_streamed_arguments(arguments: &serde_json::Value) -> Option<String> {
        let object = arguments.as_object()?;
        let raw_payload = object.get(INVALID_TOOL_ARGS_RAW_KEY)?.as_str()?;
        let parse_error = object.get(INVALID_TOOL_ARGS_ERROR_KEY)?.as_str()?;
        Some(format!(
            "invalid JSON arguments payload: {parse_error}; raw payload: {raw_payload}"
        ))
    }

    pub(crate) async fn execute_tool_call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        cancellation: &CancellationToken,
    ) -> Result<String, RuntimeError> {
        let tool = self.tool_registry.get(name).ok_or_else(|| {
            RuntimeError::Tool(ToolError::ExecutionFailed {
                tool: name.to_string(),
                message: format!("unknown tool `{name}`"),
            })
        })?;

        if let Some(message) = Self::invalid_streamed_arguments(arguments) {
            return Err(RuntimeError::Tool(ToolError::InvalidArguments {
                tool: name.to_string(),
                message,
            }));
        }

        let schema_decl = tool.schema();
        let schema_json =
            serde_json::to_value(&schema_decl.parameters).map_err(ToolError::Serialization)?;
        let validator = jsonschema::options().build(&schema_json).map_err(|error| {
            RuntimeError::Tool(ToolError::ExecutionFailed {
                tool: name.to_string(),
                message: format!("failed to compile validation schema: {error}"),
            })
        })?;

        if !validator.is_valid(arguments) {
            let error_msgs: Vec<String> = validator
                .iter_errors(arguments)
                .map(|e| format!("- {e}"))
                .collect();
            return Err(RuntimeError::Tool(ToolError::InvalidArguments {
                tool: name.to_string(),
                message: format!("schema validation failed:\n{}", error_msgs.join("\n")),
            }));
        }

        let arg_str = serde_json::to_string(arguments).map_err(ToolError::Serialization)?;

        tokio::select! {
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            timed = tokio::time::timeout(self.limits.turn_timeout, self.tool_registry.execute(name, &arg_str)) => match timed {
                Ok(output) => output.map_err(RuntimeError::from),
                Err(_) => Err(RuntimeError::BudgetExceeded),
            }
        }
    }

    pub(crate) async fn execute_tool_and_format(
        &self,
        tool_call: &ToolCall,
        cancellation: &CancellationToken,
    ) -> Result<Message, RuntimeError> {
        let output = match self
            .execute_tool_call(&tool_call.name, &tool_call.arguments, cancellation)
            .await
        {
            Ok(out) => out,
            Err(RuntimeError::Tool(err)) => {
                tracing::warn!(
                    ?err,
                    tool = tool_call.name,
                    "tool execution failed, injecting error for self-correction"
                );
                format!("Tool execution failed: {}", err)
            }
            Err(e) => return Err(e),
        };
        let output = scrub_tool_output(&output);

        Ok(Message {
            role: MessageRole::Tool,
            content: Some(output),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call.id.clone()),
        })
    }
}
