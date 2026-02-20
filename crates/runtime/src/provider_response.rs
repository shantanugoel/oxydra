use super::*;

impl AgentRuntime {
    pub(crate) async fn request_provider_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        let caps = self.provider.capabilities(&context.model)?;
        if caps.supports_streaming {
            match self.stream_response(context, cancellation).await {
                Ok(response) => return Ok(response),
                Err(StreamCollectError::Cancelled) => return Err(RuntimeError::Cancelled),
                Err(StreamCollectError::TurnTimedOut) => return Err(RuntimeError::BudgetExceeded),
                Err(error) => {
                    tracing::debug!(?error, "streaming path failed; falling back to complete");
                }
            }
        }
        self.complete_response(context, cancellation).await
    }

    pub(crate) async fn stream_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, StreamCollectError> {
        if cancellation.is_cancelled() {
            return Err(StreamCollectError::Cancelled);
        }

        let mut stream = tokio::select! {
            _ = cancellation.cancelled() => return Err(StreamCollectError::Cancelled),
            timed = tokio::time::timeout(
                self.limits.turn_timeout,
                self.provider.stream(context, self.stream_buffer_size),
            ) => match timed {
                Ok(stream) => stream.map_err(StreamCollectError::Provider)?,
                Err(_) => return Err(StreamCollectError::TurnTimedOut),
            },
        };
        let mut text_buffer = String::new();
        let mut tool_calls = ToolCallAccumulator::default();
        let mut finish_reason = None;
        let mut usage = None;

        loop {
            let item = tokio::select! {
                _ = cancellation.cancelled() => return Err(StreamCollectError::Cancelled),
                timed = tokio::time::timeout(self.limits.turn_timeout, stream.recv()) => match timed {
                    Ok(item) => item,
                    Err(_) => return Err(StreamCollectError::TurnTimedOut),
                }
            };
            let Some(item) = item else {
                break;
            };

            match item {
                Ok(StreamItem::Text(text)) => text_buffer.push_str(&text),
                Ok(StreamItem::ToolCallDelta(delta)) => tool_calls.merge(delta),
                Ok(StreamItem::FinishReason(reason)) => finish_reason = Some(reason),
                Ok(StreamItem::UsageUpdate(update)) => {
                    usage = Some(Self::merge_usage(usage.take(), update));
                }
                Ok(StreamItem::ConnectionLost(message)) => {
                    return Err(StreamCollectError::ConnectionLost(message));
                }
                Ok(StreamItem::ReasoningDelta(_)) => {}
                Err(error) => return Err(StreamCollectError::Provider(error)),
            }
        }

        let tool_calls = tool_calls
            .build(&context.provider)
            .map_err(StreamCollectError::Provider)?;
        let content = (!text_buffer.is_empty()).then_some(text_buffer);
        let message = Message {
            role: MessageRole::Assistant,
            content,
            tool_calls: tool_calls.clone(),
            tool_call_id: None,
        };
        Ok(Response {
            message,
            tool_calls,
            finish_reason,
            usage,
        })
    }

    pub(crate) async fn complete_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
    ) -> Result<Response, RuntimeError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeError::Cancelled);
        }
        tokio::select! {
            _ = cancellation.cancelled() => Err(RuntimeError::Cancelled),
            timed = tokio::time::timeout(self.limits.turn_timeout, self.provider.complete(context)) => match timed {
                Ok(response) => response.map_err(RuntimeError::from),
                Err(_) => Err(RuntimeError::BudgetExceeded),
            }
        }
    }
}
