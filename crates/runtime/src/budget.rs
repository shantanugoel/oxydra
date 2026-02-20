use super::*;

impl AgentRuntime {
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
