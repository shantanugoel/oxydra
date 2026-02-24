use super::{
    scrubbing::{scrub_host_paths, scrub_tool_output, translate_tool_arg_paths},
    *,
};

/// Recursively strip `additionalProperties` from every object node in a JSON Schema value
/// so the runtime validator accepts extra properties the LLM may have injected.
fn strip_additional_properties(schema: &mut serde_json::Value) {
    if let Some(obj) = schema.as_object_mut() {
        obj.remove("additionalProperties");
        if let Some(props) = obj.get_mut("properties")
            && let Some(props_obj) = props.as_object_mut()
        {
            for v in props_obj.values_mut() {
                strip_additional_properties(v);
            }
        }
        if let Some(items) = obj.get_mut("items") {
            strip_additional_properties(items);
        }
    }
}

/// Remove `null`-valued keys from `args` when they are not listed in the schema's
/// `required` array.  LLMs often send `null` for optional parameters; the schema
/// type (e.g. `"string"`) would reject `null`, but the Rust deserializer treats
/// `null` the same as the field being absent.
fn strip_null_optional_fields(args: &mut serde_json::Value, schema: &serde_json::Value) {
    let (Some(obj), Some(schema_obj)) = (args.as_object_mut(), schema.as_object()) else {
        return;
    };
    let required: Vec<&str> = schema_obj
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let null_optional_keys: Vec<String> = obj
        .iter()
        .filter(|(k, v)| v.is_null() && !required.contains(&k.as_str()))
        .map(|(k, _)| k.clone())
        .collect();
    for key in null_optional_keys {
        obj.remove(&key);
    }
}

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
        let mut schema_json = schema_decl.parameters.clone();
        strip_additional_properties(&mut schema_json);

        let mut validation_args = arguments.clone();
        strip_null_optional_fields(&mut validation_args, &schema_json);

        let validator = jsonschema::options().build(&schema_json).map_err(|error| {
            RuntimeError::Tool(ToolError::ExecutionFailed {
                tool: name.to_string(),
                message: format!("failed to compile validation schema: {error}"),
            })
        })?;

        if !validator.is_valid(&validation_args) {
            let error_msgs: Vec<String> = validator
                .iter_errors(&validation_args)
                .map(|e| format!("- {e}"))
                .collect();
            return Err(RuntimeError::Tool(ToolError::InvalidArguments {
                tool: name.to_string(),
                message: format!("schema validation failed:\n{}", error_msgs.join("\n")),
            }));
        }

        // Translate virtual paths (e.g. /shared/notes.txt) to host paths
        // before the tool and security policy see them.
        let validation_args =
            translate_tool_arg_paths(name, &validation_args, &self.path_scrub_mappings);

        let arg_str = serde_json::to_string(&validation_args).map_err(ToolError::Serialization)?;
        let context = self.tool_execution_context.lock().await.clone();

        tokio::select! {
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            timed = tokio::time::timeout(self.limits.turn_timeout, self.tool_registry.execute_with_context(name, &arg_str, &context)) => match timed {
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
        tracing::debug!(tool = %tool_call.name, "executing tool call");
        let start = std::time::Instant::now();
        let output = match self
            .execute_tool_call(&tool_call.name, &tool_call.arguments, cancellation)
            .await
        {
            Ok(out) => {
                tracing::debug!(
                    tool = %tool_call.name,
                    duration_ms = start.elapsed().as_millis(),
                    "tool call succeeded"
                );
                out
            }
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
        // Scrub host paths first so the entropy scrubber does not
        // false-positive on long filesystem path segments.
        let output = scrub_host_paths(&output, &self.path_scrub_mappings);
        let output = scrub_tool_output(&output);

        Ok(Message {
            role: MessageRole::Tool,
            content: Some(output),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call.id.clone()),
        })
    }
}
