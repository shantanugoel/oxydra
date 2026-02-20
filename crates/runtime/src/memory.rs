use super::*;

impl AgentRuntime {
    pub(crate) async fn next_memory_sequence(
        &self,
        session_id: Option<&str>,
    ) -> Result<u64, RuntimeError> {
        let (Some(memory), Some(session_id)) = (&self.memory, session_id) else {
            return Ok(1);
        };

        match memory
            .recall(MemoryRecallRequest {
                session_id: session_id.to_owned(),
                limit: Some(1),
            })
            .await
        {
            Ok(records) => Ok(records
                .last()
                .map(|record| record.sequence.saturating_add(1))
                .unwrap_or(1)),
            Err(MemoryError::NotFound { .. }) => Ok(1),
            Err(error) => Err(RuntimeError::from(error)),
        }
    }

    pub(crate) async fn persist_context_tail_if_needed(
        &self,
        session_id: Option<&str>,
        next_sequence: &mut u64,
        context: &Context,
    ) -> Result<(), RuntimeError> {
        if session_id.is_none() || self.memory.is_none() {
            return Ok(());
        }

        let already_persisted = next_sequence.saturating_sub(1);
        let start_index = match usize::try_from(already_persisted) {
            Ok(index) => index,
            Err(_) => context.messages.len(),
        };
        for message in context.messages.iter().skip(start_index) {
            self.persist_message_if_needed(session_id, next_sequence, message)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn persist_message_if_needed(
        &self,
        session_id: Option<&str>,
        next_sequence: &mut u64,
        message: &Message,
    ) -> Result<(), RuntimeError> {
        let (Some(memory), Some(session_id)) = (&self.memory, session_id) else {
            return Ok(());
        };

        let payload = serde_json::to_value(message)
            .map_err(MemoryError::from)
            .map_err(RuntimeError::from)?;
        memory
            .store(MemoryStoreRequest {
                session_id: session_id.to_owned(),
                sequence: *next_sequence,
                payload,
            })
            .await
            .map_err(RuntimeError::from)?;
        *next_sequence = next_sequence.saturating_add(1);
        Ok(())
    }
}
