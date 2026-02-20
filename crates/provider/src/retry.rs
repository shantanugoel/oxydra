use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::sync::mpsc;
use types::{
    Context, ModelCatalog, Provider, ProviderError, ProviderId, ProviderStream, Response,
    StreamItem,
};

use crate::DEFAULT_STREAM_BUFFER_SIZE;

#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            backoff_base: Duration::from_millis(250),
            backoff_max: Duration::from_secs(2),
        }
    }
}

impl RetryPolicy {
    fn normalized(self) -> Self {
        let max_attempts = self.max_attempts.max(1);
        let backoff_base = if self.backoff_base.is_zero() {
            Duration::from_millis(1)
        } else {
            self.backoff_base
        };
        let backoff_max = if self.backoff_max < backoff_base {
            backoff_base
        } else {
            self.backoff_max
        };
        Self {
            max_attempts,
            backoff_base,
            backoff_max,
        }
    }

    fn backoff_delay_for_attempt(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(31);
        let factor = 1_u128 << shift;
        let base = self.backoff_base.as_millis();
        let max = self.backoff_max.as_millis();
        let delay_ms = (base.saturating_mul(factor)).min(max);
        let delay_ms_u64 = u64::try_from(delay_ms).unwrap_or(u64::MAX);
        Duration::from_millis(delay_ms_u64)
    }
}

pub struct ReliableProvider {
    inner: Arc<dyn Provider>,
    retry_policy: RetryPolicy,
}

impl ReliableProvider {
    pub fn with_defaults(inner: Box<dyn Provider>) -> Self {
        Self::new(inner, RetryPolicy::default())
    }

    pub fn new(inner: Box<dyn Provider>, retry_policy: RetryPolicy) -> Self {
        Self::from_arc(Arc::from(inner), retry_policy)
    }

    pub fn from_arc(inner: Arc<dyn Provider>, retry_policy: RetryPolicy) -> Self {
        Self {
            inner,
            retry_policy: retry_policy.normalized(),
        }
    }

    pub fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    async fn complete_with_retry(&self, context: &Context) -> Result<Response, ProviderError> {
        let mut attempt = 1_u32;
        loop {
            match self.inner.complete(context).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    if !is_retriable_provider_error(&error)
                        || attempt >= self.retry_policy.max_attempts
                    {
                        return Err(error);
                    }

                    let delay = self.retry_policy.backoff_delay_for_attempt(attempt);
                    tracing::warn!(
                        provider = %self.inner.provider_id(),
                        attempt,
                        max_attempts = self.retry_policy.max_attempts,
                        delay_ms = delay.as_millis() as u64,
                        error = %error,
                        "retrying provider completion after transient failure"
                    );
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    fn provider_id(&self) -> &ProviderId {
        self.inner.provider_id()
    }

    fn model_catalog(&self) -> &ModelCatalog {
        self.inner.model_catalog()
    }

    async fn complete(&self, context: &Context) -> Result<Response, ProviderError> {
        self.complete_with_retry(context).await
    }

    async fn stream(
        &self,
        context: &Context,
        buffer_size: usize,
    ) -> Result<ProviderStream, ProviderError> {
        let channel_size = if buffer_size == 0 {
            DEFAULT_STREAM_BUFFER_SIZE
        } else {
            buffer_size
        };
        let (sender, receiver) = mpsc::channel(channel_size);
        let context = context.clone();
        let inner = Arc::clone(&self.inner);
        let retry_policy = self.retry_policy;

        tokio::spawn(async move {
            let mut attempt = 1_u32;
            'retry_stream: loop {
                let mut stream = match inner.stream(&context, buffer_size).await {
                    Ok(stream) => stream,
                    Err(error) => {
                        if !is_retriable_provider_error(&error)
                            || attempt >= retry_policy.max_attempts
                        {
                            let _ = sender.send(Err(error)).await;
                            return;
                        }

                        let delay = retry_policy.backoff_delay_for_attempt(attempt);
                        tracing::warn!(
                            provider = %inner.provider_id(),
                            attempt,
                            max_attempts = retry_policy.max_attempts,
                            delay_ms = delay.as_millis() as u64,
                            error = %error,
                            "retrying provider stream setup after transient failure"
                        );
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                };

                while let Some(item) = stream.recv().await {
                    match item {
                        Ok(StreamItem::ConnectionLost(message)) => {
                            if attempt >= retry_policy.max_attempts {
                                let _ = sender.send(Ok(StreamItem::ConnectionLost(message))).await;
                                return;
                            }

                            let delay = retry_policy.backoff_delay_for_attempt(attempt);
                            tracing::warn!(
                                provider = %inner.provider_id(),
                                attempt,
                                max_attempts = retry_policy.max_attempts,
                                delay_ms = delay.as_millis() as u64,
                                message = %message,
                                "retrying provider stream after connection loss"
                            );
                            attempt += 1;
                            tokio::time::sleep(delay).await;
                            continue 'retry_stream;
                        }
                        other => {
                            if sender.send(other).await.is_err() {
                                return;
                            }
                        }
                    }
                }

                return;
            }
        });

        Ok(receiver)
    }
}

fn is_retriable_provider_error(error: &ProviderError) -> bool {
    match error {
        ProviderError::Transport { .. } => true,
        ProviderError::HttpStatus { status, .. } => *status == 429 || *status >= 500,
        _ => false,
    }
}
