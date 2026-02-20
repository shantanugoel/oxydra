use std::sync::OnceLock;

use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ContextBudgetBreakdown {
    pub(crate) max_context_tokens: u64,
    pub(crate) system_tokens: u64,
    pub(crate) retrieved_memory_tokens: u64,
    pub(crate) history_tokens: u64,
    pub(crate) tool_schema_tokens: u64,
    pub(crate) safety_buffer_tokens: u64,
}

impl ContextBudgetBreakdown {
    pub(crate) fn total_tokens(self) -> u64 {
        self.system_tokens
            .saturating_add(self.retrieved_memory_tokens)
            .saturating_add(self.history_tokens)
            .saturating_add(self.tool_schema_tokens)
            .saturating_add(self.safety_buffer_tokens)
    }
}

impl AgentRuntime {
    pub(crate) async fn prepare_provider_context(
        &self,
        session_id: Option<&str>,
        context: &Context,
    ) -> Result<Context, RuntimeError> {
        let max_context_tokens = self.resolve_max_context_tokens(context)?;
        let safety_buffer_tokens = self.limits.context_budget.safety_buffer_tokens;
        if self.memory_retrieval.is_none()
            && self.is_obviously_within_budget(context, max_context_tokens, safety_buffer_tokens)
        {
            return Ok(context.clone());
        }

        let tool_schema_tokens = self.estimate_tool_schema_tokens(context)?;
        let static_tokens = tool_schema_tokens.saturating_add(safety_buffer_tokens);
        if static_tokens > max_context_tokens {
            return Err(RuntimeError::BudgetExceeded);
        }

        let retrieval_message = self
            .fetch_retrieved_memory_message(session_id, context)
            .await?;
        let mut retrieved_messages = Vec::new();
        let mut retrieved_memory_tokens = 0_u64;
        if let Some(message) = retrieval_message {
            let candidate_tokens = self.estimate_message_tokens(&message)?;
            if static_tokens.saturating_add(candidate_tokens) <= max_context_tokens {
                retrieved_memory_tokens = candidate_tokens;
                retrieved_messages.push(message);
            }
        }

        let system_messages: Vec<Message> = context
            .messages
            .iter()
            .filter(|message| message.role == MessageRole::System)
            .cloned()
            .collect();
        let history_messages: Vec<Message> = context
            .messages
            .iter()
            .filter(|message| message.role != MessageRole::System)
            .cloned()
            .collect();

        let message_budget = max_context_tokens
            .saturating_sub(static_tokens)
            .saturating_sub(retrieved_memory_tokens);
        let (selected_system, system_tokens) =
            self.select_messages_within_budget(&system_messages, message_budget, 0)?;
        let (selected_history, history_tokens) =
            self.select_messages_within_budget(&history_messages, message_budget, system_tokens)?;

        let mut provider_context = context.clone();
        provider_context.messages = selected_system;
        provider_context.messages.extend(retrieved_messages);
        provider_context.messages.extend(selected_history);

        let breakdown = ContextBudgetBreakdown {
            max_context_tokens,
            system_tokens,
            retrieved_memory_tokens,
            history_tokens,
            tool_schema_tokens,
            safety_buffer_tokens,
        };
        let utilization = if breakdown.max_context_tokens == 0 {
            0.0
        } else {
            breakdown.total_tokens() as f64 / breakdown.max_context_tokens as f64
        };
        tracing::debug!(
            max_context_tokens = breakdown.max_context_tokens,
            system_tokens = breakdown.system_tokens,
            retrieved_memory_tokens = breakdown.retrieved_memory_tokens,
            history_tokens = breakdown.history_tokens,
            tool_schema_tokens = breakdown.tool_schema_tokens,
            safety_buffer_tokens = breakdown.safety_buffer_tokens,
            utilization,
            trigger_ratio = self.limits.context_budget.trigger_ratio,
            "prepared provider context within token budget"
        );

        if breakdown.total_tokens() > max_context_tokens {
            return Err(RuntimeError::BudgetExceeded);
        }

        Ok(provider_context)
    }

    fn resolve_max_context_tokens(&self, context: &Context) -> Result<u64, RuntimeError> {
        let caps = self.provider.capabilities(&context.model)?;
        let max_context = caps
            .max_context_tokens
            .unwrap_or(self.limits.context_budget.fallback_max_context_tokens);
        Ok(u64::from(max_context))
    }

    fn is_obviously_within_budget(
        &self,
        context: &Context,
        max_context_tokens: u64,
        safety_buffer_tokens: u64,
    ) -> bool {
        let available_tokens = max_context_tokens.saturating_sub(safety_buffer_tokens);
        if available_tokens == 0 {
            return false;
        }

        let rough_char_budget = match usize::try_from(available_tokens.saturating_mul(3)) {
            Ok(value) => value,
            Err(_) => return false,
        };
        let message_chars = context
            .messages
            .iter()
            .map(|message| message.content.as_ref().map_or(0, String::len))
            .sum::<usize>();
        let tool_chars = serde_json::to_string(&context.tools)
            .map(|value| value.len())
            .unwrap_or(usize::MAX);

        message_chars.saturating_add(tool_chars) <= rough_char_budget
    }

    async fn fetch_retrieved_memory_message(
        &self,
        session_id: Option<&str>,
        context: &Context,
    ) -> Result<Option<Message>, RuntimeError> {
        let (Some(memory), Some(session_id)) = (&self.memory_retrieval, session_id) else {
            return Ok(None);
        };
        let Some(query) = Self::latest_retrieval_query(context) else {
            return Ok(None);
        };

        let snippets = memory
            .hybrid_query(MemoryHybridQueryRequest {
                session_id: session_id.to_owned(),
                query,
                query_embedding: None,
                top_k: Some(self.limits.retrieval.top_k),
                vector_weight: Some(self.limits.retrieval.vector_weight),
                fts_weight: Some(self.limits.retrieval.fts_weight),
            })
            .await
            .map_err(RuntimeError::from)?;
        if snippets.is_empty() {
            return Ok(None);
        }

        let mut memory_block = String::from("Retrieved memory snippets:\n");
        for snippet in snippets {
            let normalized = snippet
                .text
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let truncated = if normalized.chars().count() > 240 {
                format!("{}...", normalized.chars().take(240).collect::<String>())
            } else {
                normalized
            };
            memory_block.push_str("- [");
            memory_block.push_str(snippet.chunk_id.as_str());
            memory_block.push_str(" score=");
            memory_block.push_str(format!("{:.3}", snippet.score).as_str());
            memory_block.push_str("] ");
            memory_block.push_str(truncated.as_str());
            memory_block.push('\n');
        }
        memory_block.truncate(memory_block.trim_end().len());

        Ok(Some(Message {
            role: MessageRole::System,
            content: Some(memory_block),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }))
    }

    fn latest_retrieval_query(context: &Context) -> Option<String> {
        let from_latest_user = context.messages.iter().rev().find_map(|message| {
            (message.role == MessageRole::User)
                .then_some(message.content.as_deref().unwrap_or_default().trim())
                .filter(|content| !content.is_empty())
                .map(str::to_owned)
        });
        if from_latest_user.is_some() {
            return from_latest_user;
        }

        context.messages.iter().rev().find_map(|message| {
            message
                .content
                .as_deref()
                .map(str::trim)
                .filter(|content| !content.is_empty())
                .map(str::to_owned)
        })
    }

    fn select_messages_within_budget(
        &self,
        messages: &[Message],
        budget: u64,
        already_used: u64,
    ) -> Result<(Vec<Message>, u64), RuntimeError> {
        if already_used >= budget {
            return Ok((Vec::new(), 0));
        }

        let mut selected_rev = Vec::new();
        let mut used = 0_u64;
        for message in messages.iter().rev() {
            let message_tokens = self.estimate_message_tokens(message)?;
            let next_total = already_used
                .saturating_add(used)
                .saturating_add(message_tokens);
            if next_total <= budget {
                used = used.saturating_add(message_tokens);
                selected_rev.push(message.clone());
            }
        }
        selected_rev.reverse();
        Ok((selected_rev, used))
    }

    fn estimate_tool_schema_tokens(&self, context: &Context) -> Result<u64, RuntimeError> {
        let serialized =
            serde_json::to_string(&context.tools).map_err(|_| RuntimeError::BudgetExceeded)?;
        Self::estimate_text_tokens(serialized.as_str())
    }

    fn estimate_message_tokens(&self, message: &Message) -> Result<u64, RuntimeError> {
        let serialized =
            serde_json::to_string(message).map_err(|_| RuntimeError::BudgetExceeded)?;
        Self::estimate_text_tokens(serialized.as_str())
    }

    fn estimate_text_tokens(text: &str) -> Result<u64, RuntimeError> {
        static TOKENIZER: OnceLock<Result<tiktoken_rs::CoreBPE, String>> = OnceLock::new();
        let tokenizer = match TOKENIZER
            .get_or_init(|| tiktoken_rs::cl100k_base().map_err(|error| error.to_string()))
        {
            Ok(tokenizer) => tokenizer,
            Err(error) => {
                tracing::error!(%error, "failed to initialize context budget tokenizer");
                return Err(RuntimeError::BudgetExceeded);
            }
        };

        let encoded = tokenizer.encode_with_special_tokens(text);
        u64::try_from(encoded.len()).map_err(|_| RuntimeError::BudgetExceeded)
    }

    pub(crate) fn validate_guard_preconditions(&self) -> Result<(), RuntimeError> {
        if self.limits.turn_timeout.is_zero() || self.limits.max_turns == 0 {
            return Err(RuntimeError::BudgetExceeded);
        }
        if self
            .limits
            .max_cost
            .is_some_and(|max_cost| !max_cost.is_finite() || max_cost <= 0.0)
        {
            return Err(RuntimeError::BudgetExceeded);
        }
        Ok(())
    }

    pub(crate) fn enforce_cost_budget(
        &self,
        usage: Option<&UsageUpdate>,
        accumulated_cost: &mut f64,
    ) -> Result<(), RuntimeError> {
        let Some(max_cost) = self.limits.max_cost else {
            return Ok(());
        };
        // Phase 5 interim accounting: use provider-reported token usage as cost units.
        let turn_cost = usage
            .and_then(Self::usage_to_cost)
            .ok_or(RuntimeError::BudgetExceeded)?;
        *accumulated_cost += turn_cost;
        if *accumulated_cost > max_cost {
            return Err(RuntimeError::BudgetExceeded);
        }
        Ok(())
    }

    pub(crate) fn usage_to_cost(usage: &UsageUpdate) -> Option<f64> {
        let total_tokens = usage.total_tokens.or_else(|| {
            let prompt = usage.prompt_tokens.unwrap_or(0);
            let completion = usage.completion_tokens.unwrap_or(0);
            let aggregated = prompt.saturating_add(completion);
            (aggregated > 0).then_some(aggregated)
        })?;
        Some(total_tokens as f64)
    }

    pub(crate) fn merge_usage(existing: Option<UsageUpdate>, update: UsageUpdate) -> UsageUpdate {
        let mut merged = existing.unwrap_or_default();
        if update.prompt_tokens.is_some() {
            merged.prompt_tokens = update.prompt_tokens;
        }
        if update.completion_tokens.is_some() {
            merged.completion_tokens = update.completion_tokens;
        }
        if update.total_tokens.is_some() {
            merged.total_tokens = update.total_tokens;
        }
        merged
    }
}
