use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::{
    Context, ModelCatalog, ModelId, ProviderCaps, ProviderError, ProviderId, Response, StreamItem,
    model::derive_caps,
};

pub type ProviderStream = mpsc::Receiver<Result<StreamItem, ProviderError>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn provider_id(&self) -> &ProviderId;

    /// Returns the catalog provider namespace used for model validation.
    ///
    /// By default this returns the same as `provider_id()`. Providers whose
    /// instance ID differs from their catalog namespace (e.g. `ResponsesProvider`
    /// with instance ID `"openai-responses"` but catalog namespace `"openai"`)
    /// must override this method.
    fn catalog_provider_id(&self) -> &ProviderId {
        self.provider_id()
    }

    fn model_catalog(&self) -> &ModelCatalog;

    fn capabilities(&self, model: &ModelId) -> Result<ProviderCaps, ProviderError> {
        let catalog = self.model_catalog();
        let catalog_provider = self.catalog_provider_id();
        let descriptor = catalog.validate(catalog_provider, model)?;
        Ok(derive_caps(
            catalog_provider.0.as_str(),
            descriptor,
            &catalog.caps_overrides,
        ))
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError>;

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError>;
}
