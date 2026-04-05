use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures::FutureExt;
use memory_crate::SchedulerStore;
use memory_crate::cadence::next_run_for_cadence;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use types::{
    ChannelCapabilities, GatewayMediaAttachment, GatewayScheduledNotification, GatewayServerFrame,
    GatewaySession, MediaAttachment, NotificationPolicy, ScheduleCadence, ScheduleDefinition,
    ScheduleRunRecord, ScheduleRunStatus, ScheduleStatus, SchedulerConfig,
};

use crate::ScheduledTurnRunner;

/// Callback trait for publishing scheduled notifications to connected users.
#[async_trait]
pub trait SchedulerNotifier: Send + Sync {
    async fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame);
}

pub struct SchedulerExecutor {
    store: Arc<dyn SchedulerStore>,
    turn_runner: Arc<dyn ScheduledTurnRunner>,
    notifier: Arc<dyn SchedulerNotifier>,
    config: SchedulerConfig,
    cancellation: CancellationToken,
}

const OUTPUT_SUMMARY_LIMIT_CHARS: usize = 500;

impl SchedulerExecutor {
    pub fn new(
        store: Arc<dyn SchedulerStore>,
        turn_runner: Arc<dyn ScheduledTurnRunner>,
        notifier: Arc<dyn SchedulerNotifier>,
        config: SchedulerConfig,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            store,
            turn_runner,
            notifier,
            config,
            cancellation,
        }
    }

    pub async fn run(&self) {
        let mut interval =
            tokio::time::interval(Duration::from_secs(self.config.poll_interval_secs));
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = self.cancellation.cancelled() => {
                    tracing::info!("scheduler executor shutting down");
                    break;
                }
                _ = interval.tick() => {
                    self.tick().await;
                }
            }
        }
    }

    pub(crate) async fn tick(&self) {
        let now = Utc::now().to_rfc3339();
        let due = match self
            .store
            .due_schedules(&now, self.config.max_concurrent)
            .await
        {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!("scheduler poll failed: {e}");
                return;
            }
        };

        if due.is_empty() {
            return;
        }

        tracing::debug!("scheduler: {} due schedule(s)", due.len());

        let futs: Vec<_> = due
            .into_iter()
            .map(|schedule| self.execute_schedule(schedule))
            .collect();
        futures::future::join_all(futs).await;
    }

    async fn execute_schedule(&self, schedule: ScheduleDefinition) {
        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let panic_schedule = schedule.clone();
        let panic_run_id = run_id.clone();
        let panic_started_at = started_at.clone();

        if let Err(payload) =
            AssertUnwindSafe(self.execute_schedule_inner(schedule, run_id, started_at))
                .catch_unwind()
                .await
        {
            self.record_panicked_schedule_run(
                &panic_schedule,
                panic_run_id,
                panic_started_at,
                payload,
            )
            .await;
        }
    }

    async fn execute_schedule_inner(
        &self,
        schedule: ScheduleDefinition,
        run_id: String,
        started_at: String,
    ) {
        let session_id = format!("scheduled:{}", schedule.schedule_id);

        let prompt = if schedule.notification_policy == NotificationPolicy::Conditional {
            format!(
                "{}\n\n---\nYou are executing a scheduled task. If the result warrants notifying \
                 the user, begin your response with [NOTIFY] followed by the notification message. \
                 If no notification is needed, respond normally without [NOTIFY].",
                schedule.goal
            )
        } else {
            schedule.goal.clone()
        };

        let child_cancellation = self.cancellation.child_token();

        let channel_capabilities = schedule
            .channel_id
            .as_deref()
            .map(ChannelCapabilities::from_channel_origin);

        let result = self
            .turn_runner
            .run_scheduled_turn(
                &schedule.user_id,
                &session_id,
                prompt,
                channel_capabilities,
                child_cancellation,
            )
            .await;

        let finished_at = Utc::now().to_rfc3339();

        let (status, response_text, media_items) = match result {
            Ok((text, media)) => (ScheduleRunStatus::Success, text, media),
            Err(types::RuntimeError::Cancelled) => {
                (ScheduleRunStatus::Cancelled, String::new(), Vec::new())
            }
            Err(e) => {
                tracing::warn!(
                    schedule_id = %schedule.schedule_id,
                    error = %e,
                    "scheduled turn failed"
                );
                (ScheduleRunStatus::Failed, format!("Error: {e}"), Vec::new())
            }
        };

        // Strip [NOTIFY] marker early to produce a clean text for storage and display.
        let clean_text = if schedule.notification_policy == NotificationPolicy::Conditional
            && response_text.starts_with("[NOTIFY]")
        {
            response_text
                .strip_prefix("[NOTIFY]")
                .map(|s| s.trim_start())
                .unwrap_or(&response_text)
                .to_owned()
        } else {
            response_text.clone()
        };

        let output_summary = summarize_output(&clean_text);

        let output = if clean_text.is_empty() {
            None
        } else {
            Some(clean_text.clone())
        };

        let notified = self
            .handle_notification(
                &schedule,
                &session_id,
                status,
                &response_text,
                &clean_text,
                &media_items,
            )
            .await;

        let run_record = ScheduleRunRecord {
            run_id,
            schedule_id: schedule.schedule_id.clone(),
            started_at,
            finished_at,
            status,
            output_summary,
            turn_count: 0,
            cost: 0.0,
            notified,
            output,
        };

        self.finalize_schedule_run(&schedule, run_record, &clean_text)
            .await;
    }

    async fn record_panicked_schedule_run(
        &self,
        schedule: &ScheduleDefinition,
        run_id: String,
        started_at: String,
        payload: Box<dyn Any + Send>,
    ) {
        let panic_message = panic_payload_message(payload.as_ref());
        tracing::error!(
            schedule_id = %schedule.schedule_id,
            panic_message = %panic_message,
            "scheduled execution panicked"
        );

        let clean_text = format!("Error: scheduled execution panicked: {panic_message}");
        let finished_at = Utc::now().to_rfc3339();
        let run_record = ScheduleRunRecord {
            run_id,
            schedule_id: schedule.schedule_id.clone(),
            started_at,
            finished_at,
            status: ScheduleRunStatus::Failed,
            output_summary: summarize_output(&clean_text),
            turn_count: 0,
            cost: 0.0,
            notified: false,
            output: Some(clean_text.clone()),
        };

        self.finalize_schedule_run(schedule, run_record, &clean_text)
            .await;
    }

    async fn finalize_schedule_run(
        &self,
        schedule: &ScheduleDefinition,
        run_record: ScheduleRunRecord,
        clean_text: &str,
    ) {
        let (next_run_at, new_status) =
            self.compute_reschedule(schedule, run_record.status);

        self.handle_failure_notifications(schedule, run_record.status, clean_text)
            .await;

        if let Err(e) = self
            .store
            .record_run_and_reschedule(
                &schedule.schedule_id,
                &run_record,
                next_run_at,
                new_status,
            )
            .await
        {
            tracing::error!(
                schedule_id = %schedule.schedule_id,
                error = %e,
                "failed to record run and reschedule"
            );
        }

        if let Err(e) = self
            .store
            .prune_run_history(&schedule.schedule_id, self.config.max_run_history)
            .await
        {
            tracing::warn!(
                schedule_id = %schedule.schedule_id,
                error = %e,
                "failed to prune run history"
            );
        }
    }

    async fn handle_notification(
        &self,
        schedule: &ScheduleDefinition,
        session_id: &str,
        status: ScheduleRunStatus,
        response_text: &str,
        clean_text: &str,
        media_items: &[MediaAttachment],
    ) -> bool {
        if status != ScheduleRunStatus::Success {
            return false;
        }

        let should_notify = match schedule.notification_policy {
            NotificationPolicy::Always => true,
            NotificationPolicy::Conditional => response_text.starts_with("[NOTIFY]"),
            NotificationPolicy::Never => false,
        };

        if should_notify {
            // Send media attachments before the text notification so they
            // arrive first and the text serves as a caption/summary.
            for attachment in media_items {
                self.notifier
                    .notify_user(
                        schedule,
                        GatewayServerFrame::MediaAttachment(GatewayMediaAttachment {
                            request_id: uuid::Uuid::new_v4().to_string(),
                            session: GatewaySession {
                                user_id: schedule.user_id.clone(),
                                session_id: session_id.to_owned(),
                            },
                            attachment: attachment.clone(),
                            schedule_id: Some(schedule.schedule_id.clone()),
                        }),
                    )
                    .await;
            }

            self.notifier
                .notify_user(
                    schedule,
                    GatewayServerFrame::ScheduledNotification(GatewayScheduledNotification {
                        schedule_id: schedule.schedule_id.clone(),
                        schedule_name: schedule.name.clone(),
                        message: clean_text.to_owned(),
                    }),
                )
                .await;
            return true;
        }

        false
    }

    /// Send operational failure notifications when consecutive failures
    /// hit the configured threshold or when the schedule is auto-disabled.
    async fn handle_failure_notifications(
        &self,
        schedule: &ScheduleDefinition,
        status: ScheduleRunStatus,
        clean_text: &str,
    ) {
        if status == ScheduleRunStatus::Success || status == ScheduleRunStatus::Cancelled {
            return;
        }

        let new_consecutive_failures = schedule.consecutive_failures + 1;
        let name = schedule.name.as_deref().unwrap_or(&schedule.schedule_id);
        let error_summary = truncate_with_ellipsis(clean_text, 200);

        // Notify when consecutive failures hit the threshold.
        if self.config.notify_after_failures > 0
            && new_consecutive_failures == self.config.notify_after_failures
        {
            let message = format!(
                "❌ Scheduled task '{}' has failed {} times in a row. Latest error: {}",
                name, new_consecutive_failures, error_summary
            );
            self.notifier
                .notify_user(
                    schedule,
                    GatewayServerFrame::ScheduledNotification(GatewayScheduledNotification {
                        schedule_id: schedule.schedule_id.clone(),
                        schedule_name: schedule.name.clone(),
                        message,
                    }),
                )
                .await;
        }

        // Notify when the schedule is about to be auto-disabled.
        if new_consecutive_failures >= self.config.auto_disable_after_failures {
            let message = format!(
                "⛔ Scheduled task '{}' was disabled after {} consecutive failures.",
                name, new_consecutive_failures
            );
            self.notifier
                .notify_user(
                    schedule,
                    GatewayServerFrame::ScheduledNotification(GatewayScheduledNotification {
                        schedule_id: schedule.schedule_id.clone(),
                        schedule_name: schedule.name.clone(),
                        message,
                    }),
                )
                .await;
        }
    }

    fn compute_reschedule(
        &self,
        schedule: &ScheduleDefinition,
        status: ScheduleRunStatus,
    ) -> (Option<String>, Option<ScheduleStatus>) {
        let is_one_shot = matches!(schedule.cadence, ScheduleCadence::Once { .. });

        if is_one_shot && status == ScheduleRunStatus::Success {
            return (None, Some(ScheduleStatus::Completed));
        }

        let consecutive_failures = if status == ScheduleRunStatus::Success {
            0
        } else {
            schedule.consecutive_failures + 1
        };

        if consecutive_failures >= self.config.auto_disable_after_failures {
            return (None, Some(ScheduleStatus::Disabled));
        }

        let now = Utc::now();
        match next_run_for_cadence(&schedule.cadence, now) {
            Ok(Some(next)) => (Some(next.to_rfc3339()), None),
            Ok(None) => (None, Some(ScheduleStatus::Completed)),
            Err(e) => {
                tracing::warn!(
                    schedule_id = %schedule.schedule_id,
                    error = %e,
                    "failed to compute next run; disabling schedule"
                );
                (None, Some(ScheduleStatus::Disabled))
            }
        }
    }
}

fn summarize_output(text: &str) -> Option<String> {
    if text.is_empty() {
        return None;
    }

    Some(truncate_with_ellipsis(text, OUTPUT_SUMMARY_LIMIT_CHARS))
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "panic payload unavailable".to_owned()
    }
}

pub(crate) fn truncate_with_ellipsis(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }

    if max_chars < 3 {
        return text.chars().take(max_chars).collect();
    }

    let prefix: String = text.chars().take(max_chars.saturating_sub(3)).collect();
    format!("{prefix}...")
}
