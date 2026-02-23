use super::*;

impl AgentRuntime {
    pub(crate) async fn request_provider_response(
        &self,
        context: &Context,
        cancellation: &CancellationToken,
        stream_events: Option<RuntimeStreamEventSender>,
    ) -> Result<Response, RuntimeError> {
        let caps = self.provider.capabilities(&context.model)?;
        if caps.supports_streaming {
            match self
                .stream_response(context, cancellation, stream_events.clone())
                .await
            {
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
        stream_events: Option<RuntimeStreamEventSender>,
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
                Ok(StreamItem::Text(text)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::Text(text.clone()));
                    }
                    text_buffer.push_str(&text);
                }
                Ok(StreamItem::ToolCallDelta(delta)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::ToolCallDelta(delta.clone()));
                    }
                    tool_calls.merge(delta);
                }
                Ok(StreamItem::FinishReason(reason)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::FinishReason(reason.clone()));
                    }
                    finish_reason = Some(reason);
                }
                Ok(StreamItem::UsageUpdate(update)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::UsageUpdate(update.clone()));
                    }
                    usage = Some(Self::merge_usage(usage.take(), update));
                }
                Ok(StreamItem::ConnectionLost(message)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::ConnectionLost(message.clone()));
                    }
                    return Err(StreamCollectError::ConnectionLost(message));
                }
                Ok(StreamItem::ReasoningDelta(delta)) => {
                    if let Some(stream_events) = &stream_events {
                        let _ = stream_events.send(StreamItem::ReasoningDelta(delta));
                    }
                }
                Ok(StreamItem::Progress(_)) => {
                    // Progress events are emitted by the runtime layer, not by
                    // provider streams.  If one somehow arrives here it is a
                    // no-op; it should not be forwarded from within this collect
                    // loop since the runtime already emits progress independently.
                }
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
