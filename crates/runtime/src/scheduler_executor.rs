use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use memory_crate::SchedulerStore;
use memory_crate::cadence::next_run_for_cadence;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use types::{
    GatewayScheduledNotification, GatewayServerFrame, NotificationPolicy, ScheduleCadence,
    ScheduleDefinition, ScheduleRunRecord, ScheduleRunStatus, ScheduleStatus, SchedulerConfig,
};

use crate::ScheduledTurnRunner;

/// Callback trait for publishing scheduled notifications to connected users.
pub trait SchedulerNotifier: Send + Sync {
    fn notify_user(&self, user_id: &str, frame: GatewayServerFrame);
}

pub struct SchedulerExecutor {
    store: Arc<dyn SchedulerStore>,
    turn_runner: Arc<dyn ScheduledTurnRunner>,
    notifier: Arc<dyn SchedulerNotifier>,
    config: SchedulerConfig,
    cancellation: CancellationToken,
}

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

        let futs: Vec<_> = due.into_iter().map(|s| self.execute_schedule(s)).collect();
        futures::future::join_all(futs).await;
    }

    async fn execute_schedule(&self, schedule: ScheduleDefinition) {
        let run_id = uuid::Uuid::new_v4().to_string();
        let session_id = format!("scheduled:{}", schedule.schedule_id);
        let started_at = Utc::now().to_rfc3339();

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

        let result = self
            .turn_runner
            .run_scheduled_turn(&schedule.user_id, &session_id, prompt, child_cancellation)
            .await;

        let finished_at = Utc::now().to_rfc3339();

        let (status, response_text) = match result {
            Ok(text) => (ScheduleRunStatus::Success, text),
            Err(types::RuntimeError::Cancelled) => (ScheduleRunStatus::Cancelled, String::new()),
            Err(e) => {
                tracing::warn!(
                    schedule_id = %schedule.schedule_id,
                    error = %e,
                    "scheduled turn failed"
                );
                (ScheduleRunStatus::Failed, format!("Error: {e}"))
            }
        };

        let output_summary = if response_text.is_empty() {
            None
        } else if response_text.len() > 500 {
            Some(format!("{}...", &response_text[..497]))
        } else {
            Some(response_text.clone())
        };

        let notified = self.handle_notification(&schedule, status, &response_text);

        let (next_run_at, new_status) = self.compute_reschedule(&schedule, status);

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
        };

        if let Err(e) = self
            .store
            .record_run_and_reschedule(&schedule.schedule_id, &run_record, next_run_at, new_status)
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

    fn handle_notification(
        &self,
        schedule: &ScheduleDefinition,
        status: ScheduleRunStatus,
        response_text: &str,
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
            let message = match schedule.notification_policy {
                NotificationPolicy::Conditional => response_text
                    .strip_prefix("[NOTIFY]")
                    .map(|s| s.trim_start())
                    .unwrap_or(response_text)
                    .to_owned(),
                _ => response_text.to_owned(),
            };

            self.notifier.notify_user(
                &schedule.user_id,
                GatewayServerFrame::ScheduledNotification(GatewayScheduledNotification {
                    schedule_id: schedule.schedule_id.clone(),
                    schedule_name: schedule.name.clone(),
                    message,
                }),
            );
            return true;
        }

        false
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
