use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use types::{Channel, ChannelError, ChannelHealthStatus};

pub type SharedChannel = Arc<dyn Channel>;

#[derive(Default)]
pub struct ChannelRegistry {
    channels: RwLock<BTreeMap<String, SharedChannel>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        channel_id: impl Into<String>,
        channel: SharedChannel,
    ) -> Option<SharedChannel> {
        self.write_channels().insert(channel_id.into(), channel)
    }

    pub fn get(&self, channel_id: &str) -> Option<SharedChannel> {
        self.read_channels().get(channel_id).cloned()
    }

    pub fn remove(&self, channel_id: &str) -> Option<SharedChannel> {
        self.write_channels().remove(channel_id)
    }

    pub fn channel_ids(&self) -> Vec<String> {
        self.read_channels().keys().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.read_channels().len()
    }

    pub fn is_empty(&self) -> bool {
        self.read_channels().is_empty()
    }

    pub fn snapshot(&self) -> Vec<(String, SharedChannel)> {
        self.read_channels()
            .iter()
            .map(|(channel_id, channel)| (channel_id.clone(), Arc::clone(channel)))
            .collect()
    }

    fn read_channels(&self) -> RwLockReadGuard<'_, BTreeMap<String, SharedChannel>> {
        self.channels
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_channels(&self) -> RwLockWriteGuard<'_, BTreeMap<String, SharedChannel>> {
        self.channels
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelHealthReport {
    pub channel_id: String,
    pub status: Option<ChannelHealthStatus>,
    pub error: Option<ChannelError>,
}

impl ChannelHealthReport {
    pub fn is_healthy(&self) -> bool {
        self.error.is_none() && self.status.as_ref().is_some_and(|status| status.healthy)
    }
}

pub async fn collect_channel_health(registry: &ChannelRegistry) -> Vec<ChannelHealthReport> {
    let channels = registry.snapshot();
    let mut reports = Vec::with_capacity(channels.len());

    for (channel_id, channel) in channels {
        match channel.health_check().await {
            Ok(status) => reports.push(ChannelHealthReport {
                channel_id,
                status: Some(status),
                error: None,
            }),
            Err(error) => reports.push(ChannelHealthReport {
                channel_id,
                status: None,
                error: Some(error),
            }),
        }
    }

    reports
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use types::{ChannelListenStream, ChannelOutboundEvent};

    use super::{ChannelHealthReport, ChannelRegistry, SharedChannel, collect_channel_health};
    use types::{Channel, ChannelError, ChannelHealthStatus};

    #[derive(Clone)]
    struct StaticHealthChannel {
        health_result: Result<ChannelHealthStatus, ChannelError>,
    }

    impl StaticHealthChannel {
        fn healthy(healthy: bool, message: &str) -> Self {
            Self {
                health_result: Ok(ChannelHealthStatus {
                    healthy,
                    message: Some(message.to_owned()),
                }),
            }
        }

        fn unavailable(channel: &str) -> Self {
            Self {
                health_result: Err(ChannelError::Unavailable {
                    channel: channel.to_owned(),
                }),
            }
        }
    }

    #[async_trait]
    impl Channel for StaticHealthChannel {
        async fn send(&self, _event: ChannelOutboundEvent) -> Result<(), ChannelError> {
            Ok(())
        }

        async fn listen(&self, buffer_size: usize) -> Result<ChannelListenStream, ChannelError> {
            let (_tx, rx) = mpsc::channel(buffer_size.max(1));
            Ok(rx)
        }

        async fn health_check(&self) -> Result<ChannelHealthStatus, ChannelError> {
            self.health_result.clone()
        }
    }

    #[test]
    fn registry_supports_register_lookup_and_remove() {
        let registry = ChannelRegistry::new();
        assert!(registry.is_empty());

        let first: SharedChannel = Arc::new(StaticHealthChannel::healthy(true, "ready"));
        assert!(registry.register("tui", Arc::clone(&first)).is_none());
        assert_eq!(registry.len(), 1);
        assert_eq!(registry.channel_ids(), vec!["tui".to_owned()]);

        let fetched = registry.get("tui").expect("channel should be present");
        assert!(Arc::ptr_eq(&fetched, &first));

        let replacement: SharedChannel = Arc::new(StaticHealthChannel::healthy(true, "ready"));
        let previous = registry
            .register("tui", Arc::clone(&replacement))
            .expect("register should return replaced channel");
        assert!(Arc::ptr_eq(&previous, &first));

        let removed = registry.remove("tui").expect("channel should be removable");
        assert!(Arc::ptr_eq(&removed, &replacement));
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn collect_channel_health_reports_status_and_errors() {
        let registry = ChannelRegistry::new();
        registry.register(
            "healthy",
            Arc::new(StaticHealthChannel::healthy(true, "ready")),
        );
        registry.register(
            "degraded",
            Arc::new(StaticHealthChannel::healthy(false, "maintenance")),
        );
        registry.register(
            "offline",
            Arc::new(StaticHealthChannel::unavailable("offline")),
        );

        let reports = collect_channel_health(&registry).await;
        let reports_by_id: BTreeMap<String, ChannelHealthReport> = reports
            .into_iter()
            .map(|report| (report.channel_id.clone(), report))
            .collect();

        let healthy = reports_by_id
            .get("healthy")
            .expect("healthy report should exist");
        assert!(healthy.is_healthy());
        assert_eq!(
            healthy
                .status
                .as_ref()
                .and_then(|status| status.message.as_deref()),
            Some("ready")
        );

        let degraded = reports_by_id
            .get("degraded")
            .expect("degraded report should exist");
        assert!(!degraded.is_healthy());
        assert_eq!(
            degraded
                .status
                .as_ref()
                .and_then(|status| status.message.as_deref()),
            Some("maintenance")
        );

        let offline = reports_by_id
            .get("offline")
            .expect("offline report should exist");
        assert!(!offline.is_healthy());
        assert_eq!(
            offline.error,
            Some(ChannelError::Unavailable {
                channel: "offline".to_owned(),
            })
        );
    }
}
