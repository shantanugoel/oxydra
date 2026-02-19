use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{
    Context, ModelCatalog, ModelId, ProviderCaps, ProviderError, ProviderId, Response, StreamItem,
};

pub type ProviderStream = mpsc::Receiver<Result<StreamItem, ProviderError>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn provider_id(&self) -> &ProviderId;

    fn model_catalog(&self) -> &ModelCatalog;

    fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError> {
        let descriptor = self.model_catalog().validate(self.provider_id(), model)?;
        Ok(descriptor.caps.clone())
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError>;
}
