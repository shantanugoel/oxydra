use async_trait::async_trait;

use crate::{Context, ModelCatalog, ModelId, ProviderCaps, ProviderError, ProviderId, Response};

#[async_trait]
pub trait Provider: Send + Sync {
    fn provider_id(&self) -> &ProviderId;

    fn model_catalog(&self) -> &ModelCatalog;

    fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError> {
        let descriptor = self.model_catalog().validate(self.provider_id(), model)?;
        Ok(descriptor.caps.clone())
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;
}
