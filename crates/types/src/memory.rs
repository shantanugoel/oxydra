use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::MemoryError;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryStoreRequest {
    pub session_id: String,
    pub sequence: u64,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRecallRequest {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryForgetRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRecord {
    pub session_id: String,
    pub sequence: u64,
    pub payload: Value,
}

#[async_trait]
pub trait Memory: Send + Sync {
    async fn store(&self, request: MemoryStoreRequest) -> Result<MemoryRecord, MemoryError>;

    async fn recall(&self, request: MemoryRecallRequest) -> Result<Vec<MemoryRecord>, MemoryError>;

    async fn forget(&self, request: MemoryForgetRequest) -> Result<(), MemoryError>;
}
