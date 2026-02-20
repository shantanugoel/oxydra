use async_trait::async_trait;
use types::{
    Memory, MemoryError, MemoryForgetRequest, MemoryRecallRequest, MemoryRecord, MemoryStoreRequest,
};

pub struct UnconfiguredMemory;

impl UnconfiguredMemory {
    pub fn new() -> Self {
        Self
    }

    fn backend_unavailable() -> MemoryError {
        MemoryError::Initialization {
            message:
                "memory backend is not configured; explicit local/remote selection is required"
                    .to_owned(),
        }
    }
}

impl Default for UnconfiguredMemory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Memory for UnconfiguredMemory {
    async fn store(&self, _request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn recall(
        &self,
        _request: MemoryRecallRequest,
    ) -> Result<Vec<MemoryRecord>, MemoryError> {
        Err(Self::backend_unavailable())
    }

    async fn forget(&self, _request: MemoryForgetRequest) -> Result<(), MemoryError> {
        Err(Self::backend_unavailable())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use types::{MemoryError, MemoryForgetRequest, MemoryRecallRequest, MemoryStoreRequest};

    use super::UnconfiguredMemory;
    use types::Memory;

    #[tokio::test]
    async fn unconfigured_memory_store_returns_explicit_initialization_error() {
        let memory = UnconfiguredMemory::new();
        let err = memory
            .store(MemoryStoreRequest {
                session_id: "session-1".to_owned(),
                sequence: 1,
                payload: json!({"message":"hello"}),
            })
            .await
            .expect_err("unconfigured backend must not silently store");
        assert!(matches!(err, MemoryError::Initialization { .. }));
    }

    #[tokio::test]
    async fn unconfigured_memory_recall_returns_explicit_initialization_error() {
        let memory = UnconfiguredMemory::new();
        let err = memory
            .recall(MemoryRecallRequest {
                session_id: "session-1".to_owned(),
                limit: Some(10),
            })
            .await
            .expect_err("unconfigured backend must not silently recall");
        assert!(matches!(err, MemoryError::Initialization { .. }));
    }

    #[tokio::test]
    async fn unconfigured_memory_forget_returns_explicit_initialization_error() {
        let memory = UnconfiguredMemory::new();
        let err = memory
            .forget(MemoryForgetRequest {
                session_id: "session-1".to_owned(),
            })
            .await
            .expect_err("unconfigured backend must not silently forget");
        assert!(matches!(err, MemoryError::Initialization { .. }));
    }
}
