use serde::{Deserialize, Serialize};

// --- Schedule cadence ---

/// How/when a schedule fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ScheduleCadence {
    /// Fire once at a specific UTC timestamp.
    Once { at: String },
    /// Fire periodically according to a cron expression (5-field standard).
    Cron {
        expression: String,
        timezone: String,
    },
    /// Fire at a fixed interval (minimum 60s).
    Interval { every_secs: u64 },
}

// --- Notification policy ---

/// How the scheduler routes output after a scheduled turn completes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationPolicy {
    /// Always send the response to the user's active channel.
    #[default]
    Always,
    /// The LLM response is inspected for a notification signal.
    Conditional,
    /// Never notify — results stored in memory only.
    Never,
}

// --- Schedule status ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleStatus {
    Active,
    Paused,
    /// One-shot that has fired.
    Completed,
    /// Auto-disabled after consecutive failures.
    Disabled,
}

// --- Schedule run status ---

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleRunStatus {
    Success,
    Failed,
    Cancelled,
}

// --- Schedule definition (persisted) ---

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleDefinition {
    pub schedule_id: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub goal: String,
    pub cadence: ScheduleCadence,
    pub notification_policy: NotificationPolicy,
    pub status: ScheduleStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_status: Option<ScheduleRunStatus>,
    pub consecutive_failures: u32,
}

// --- Execution record (audit trail) ---

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleRunRecord {
    pub run_id: String,
    pub schedule_id: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: ScheduleRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_summary: Option<String>,
    pub turn_count: u32,
    pub cost: f64,
    pub notified: bool,
}

// --- Search types ---

/// Filters for schedule_search. All fields are optional.
#[derive(Debug, Clone, Default)]
pub struct ScheduleSearchFilters {
    pub name_contains: Option<String>,
    pub status: Option<ScheduleStatus>,
    pub cadence_type: Option<String>,
    pub notification_policy: Option<NotificationPolicy>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone)]
pub struct ScheduleSearchResult {
    pub schedules: Vec<ScheduleDefinition>,
    pub total_count: usize,
    pub offset: usize,
    pub limit: usize,
}

/// Patch for schedule_edit. Only `Some` fields are updated.
#[derive(Debug, Clone, Default)]
pub struct SchedulePatch {
    /// `Some(None)` clears name, `Some(Some(..))` sets it.
    pub name: Option<Option<String>>,
    pub goal: Option<String>,
    pub cadence: Option<ScheduleCadence>,
    pub notification_policy: Option<NotificationPolicy>,
    /// Active/Paused for pause/resume.
    pub status: Option<ScheduleStatus>,
    /// Recomputed next_run_at when cadence or status changes.
    pub next_run_at: Option<Option<String>>,
    pub updated_at: Option<String>,
    pub consecutive_failures: Option<u32>,
}
