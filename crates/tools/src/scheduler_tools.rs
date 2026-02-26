use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::Utc;
use memory::{
    SchedulerStore, format_in_timezone, next_run_for_cadence, parse_cadence, validate_cadence,
};
use serde::Deserialize;
use serde_json::json;
use types::{
    FunctionDecl, NotificationPolicy, SafetyTier, ScheduleCadence, ScheduleDefinition,
    SchedulePatch, ScheduleSearchFilters, ScheduleStatus, SchedulerConfig, Tool, ToolError,
    ToolExecutionContext,
};

use crate::{execution_failed, invalid_args, parse_args};

pub const SCHEDULE_CREATE_TOOL_NAME: &str = "schedule_create";
pub const SCHEDULE_SEARCH_TOOL_NAME: &str = "schedule_search";
pub const SCHEDULE_EDIT_TOOL_NAME: &str = "schedule_edit";
pub const SCHEDULE_DELETE_TOOL_NAME: &str = "schedule_delete";
pub const SCHEDULE_RUNS_TOOL_NAME: &str = "schedule_runs";
pub const SCHEDULE_RUN_OUTPUT_TOOL_NAME: &str = "schedule_run_output";

const SEARCH_DEFAULT_LIMIT: usize = 20;
const SEARCH_MAX_LIMIT: usize = 50;

// ---------------------------------------------------------------------------
// Argument structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ScheduleCreateArgs {
    name: Option<String>,
    goal: String,
    cadence_type: String,
    cadence_value: String,
    timezone: Option<String>,
    notification: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScheduleSearchArgs {
    name: Option<String>,
    status: Option<String>,
    cadence_type: Option<String>,
    notification: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ScheduleEditArgs {
    schedule_id: String,
    name: Option<String>,
    goal: Option<String>,
    cadence_type: Option<String>,
    cadence_value: Option<String>,
    timezone: Option<String>,
    notification: Option<String>,
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScheduleDeleteArgs {
    schedule_id: String,
}

#[derive(Debug, Deserialize)]
struct ScheduleRunsArgs {
    schedule_id: String,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ScheduleRunOutputArgs {
    run_id: String,
    offset: Option<usize>,
    max_chars: Option<usize>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn resolve_user_id(context: &ToolExecutionContext, tool_name: &str) -> Result<String, ToolError> {
    context
        .user_id
        .clone()
        .ok_or_else(|| execution_failed(tool_name, "user context is not available"))
}

fn parse_notification_policy(s: &str, tool_name: &str) -> Result<NotificationPolicy, ToolError> {
    match s {
        "always" => Ok(NotificationPolicy::Always),
        "conditional" => Ok(NotificationPolicy::Conditional),
        "never" => Ok(NotificationPolicy::Never),
        _ => Err(invalid_args(
            tool_name,
            format!("invalid notification value: {s}; expected always, conditional, or never"),
        )),
    }
}

fn parse_schedule_status(s: &str, tool_name: &str) -> Result<ScheduleStatus, ToolError> {
    match s {
        "active" => Ok(ScheduleStatus::Active),
        "paused" => Ok(ScheduleStatus::Paused),
        _ => Err(invalid_args(
            tool_name,
            format!("invalid status value: {s}; expected active or paused"),
        )),
    }
}

fn schedule_to_json(def: &ScheduleDefinition) -> serde_json::Value {
    let next_run_local = def
        .next_run_at
        .as_deref()
        .and_then(|ts| format_in_timezone(ts, &def.cadence));
    json!({
        "schedule_id": def.schedule_id,
        "name": def.name,
        "goal": def.goal,
        "cadence": def.cadence,
        "notification_policy": def.notification_policy,
        "status": def.status,
        "created_at": def.created_at,
        "updated_at": def.updated_at,
        "next_run_at": def.next_run_at,
        "next_run_local": next_run_local,
        "last_run_at": def.last_run_at,
        "last_run_status": def.last_run_status,
        "consecutive_failures": def.consecutive_failures,
    })
}

// ---------------------------------------------------------------------------
// ScheduleCreateTool
// ---------------------------------------------------------------------------

pub struct ScheduleCreateTool {
    store: Arc<dyn SchedulerStore>,
    max_schedules_per_user: usize,
    min_interval_secs: u64,
    default_timezone: String,
}

impl ScheduleCreateTool {
    pub fn new(
        store: Arc<dyn SchedulerStore>,
        max_schedules_per_user: usize,
        min_interval_secs: u64,
        default_timezone: String,
    ) -> Self {
        Self {
            store,
            max_schedules_per_user,
            min_interval_secs,
            default_timezone,
        }
    }
}

#[async_trait]
impl Tool for ScheduleCreateTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_CREATE_TOOL_NAME,
            Some(
                "Create a new scheduled task. The task will run automatically according to the \
                 specified cadence (cron expression, one-time timestamp, or fixed interval). \
                 Use this when the user wants something done periodically or at a specific \
                 future time."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["goal", "cadence_type", "cadence_value"],
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Optional human-readable name for the schedule"
                    },
                    "goal": {
                        "type": "string",
                        "description": "What the scheduled task should accomplish (the prompt for each run)",
                        "minLength": 1
                    },
                    "cadence_type": {
                        "type": "string",
                        "enum": ["cron", "once", "interval"],
                        "description": "Type of schedule cadence"
                    },
                    "cadence_value": {
                        "type": "string",
                        "description": "Cadence value: cron expression (5-field), RFC3339 timestamp for once, or interval in seconds",
                        "minLength": 1
                    },
                    "timezone": {
                        "type": "string",
                        "description": "IANA timezone for cron schedules (e.g. 'America/New_York'). Defaults to server config."
                    },
                    "notification": {
                        "type": "string",
                        "enum": ["always", "conditional", "never"],
                        "description": "When to notify the user about results. Default: always"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleCreateArgs = parse_args(SCHEDULE_CREATE_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_CREATE_TOOL_NAME)?;

        // Check per-user limit.
        let count = self
            .store
            .count_schedules(&user_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))?;
        if count >= self.max_schedules_per_user {
            return Err(execution_failed(
                SCHEDULE_CREATE_TOOL_NAME,
                format!(
                    "you have reached the maximum number of schedules ({count}/{})",
                    self.max_schedules_per_user
                ),
            ));
        }

        let timezone = request
            .timezone
            .unwrap_or_else(|| self.default_timezone.clone());

        // Parse cadence.
        let cadence = parse_cadence(&request.cadence_type, &request.cadence_value, &timezone)
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))?;

        // Validate cadence.
        validate_cadence(&cadence, self.min_interval_secs)
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))?;

        // Compute next run.
        let next_run = next_run_for_cadence(&cadence, Utc::now())
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))?;

        let notification_policy = match request.notification.as_deref() {
            Some(n) => parse_notification_policy(n, SCHEDULE_CREATE_TOOL_NAME)?,
            None => NotificationPolicy::default(),
        };

        let now = Utc::now().to_rfc3339();
        let schedule_id = uuid::Uuid::new_v4().to_string();

        let def = ScheduleDefinition {
            schedule_id: schedule_id.clone(),
            user_id,
            name: request.name,
            goal: request.goal,
            cadence: cadence.clone(),
            notification_policy,
            status: ScheduleStatus::Active,
            created_at: now.clone(),
            updated_at: now,
            next_run_at: next_run.map(|dt| dt.to_rfc3339()),
            last_run_at: None,
            last_run_status: None,
            consecutive_failures: 0,
            channel_id: context.channel_id.clone(),
            channel_context_id: context.channel_context_id.clone(),
        };

        self.store
            .create_schedule(&def)
            .await
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))?;

        tracing::info!(
            tool = SCHEDULE_CREATE_TOOL_NAME,
            schedule_id = %schedule_id,
            "schedule created"
        );

        let result = schedule_to_json(&def);
        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_CREATE_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// ScheduleSearchTool
// ---------------------------------------------------------------------------

pub struct ScheduleSearchTool {
    store: Arc<dyn SchedulerStore>,
}

impl ScheduleSearchTool {
    pub fn new(store: Arc<dyn SchedulerStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ScheduleSearchTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_SEARCH_TOOL_NAME,
            Some(
                "Search and list your scheduled tasks. Use this to find schedules by name, \
                 status, cadence type, or notification policy. Returns paginated results."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Filter by schedule name (substring match)"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["active", "paused", "completed", "disabled"],
                        "description": "Filter by schedule status"
                    },
                    "cadence_type": {
                        "type": "string",
                        "enum": ["cron", "once", "interval"],
                        "description": "Filter by cadence type"
                    },
                    "notification": {
                        "type": "string",
                        "enum": ["always", "conditional", "never"],
                        "description": "Filter by notification policy"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results to return (default 20, max 50)",
                        "minimum": 1,
                        "maximum": 50,
                        "default": 20
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Number of results to skip (default 0)",
                        "minimum": 0,
                        "default": 0
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleSearchArgs = parse_args(SCHEDULE_SEARCH_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_SEARCH_TOOL_NAME)?;

        let limit = request
            .limit
            .unwrap_or(SEARCH_DEFAULT_LIMIT)
            .min(SEARCH_MAX_LIMIT);
        let offset = request.offset.unwrap_or(0);

        let status = request
            .status
            .as_deref()
            .map(|s| match s {
                "active" => Ok(ScheduleStatus::Active),
                "paused" => Ok(ScheduleStatus::Paused),
                "completed" => Ok(ScheduleStatus::Completed),
                "disabled" => Ok(ScheduleStatus::Disabled),
                _ => Err(invalid_args(
                    SCHEDULE_SEARCH_TOOL_NAME,
                    format!("invalid status filter: {s}"),
                )),
            })
            .transpose()?;

        let notification_policy = request
            .notification
            .as_deref()
            .map(|n| parse_notification_policy(n, SCHEDULE_SEARCH_TOOL_NAME))
            .transpose()?;

        let filters = ScheduleSearchFilters {
            name_contains: request.name,
            status,
            cadence_type: request.cadence_type,
            notification_policy,
            limit,
            offset,
        };

        let search_result = self
            .store
            .search_schedules(&user_id, &filters)
            .await
            .map_err(|e| execution_failed(SCHEDULE_SEARCH_TOOL_NAME, e.to_string()))?;

        let schedules: Vec<serde_json::Value> = search_result
            .schedules
            .iter()
            .map(schedule_to_json)
            .collect();

        let remaining = search_result
            .total_count
            .saturating_sub(offset + schedules.len());

        let mut result = json!({
            "schedules": schedules,
            "total": search_result.total_count,
            "offset": offset,
            "limit": limit,
            "remaining": remaining,
        });

        if remaining > 0 {
            result["hint"] = json!(format!(
                "Use offset={} to see the next page.",
                offset + limit
            ));
        }

        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_SEARCH_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// ScheduleEditTool
// ---------------------------------------------------------------------------

pub struct ScheduleEditTool {
    store: Arc<dyn SchedulerStore>,
    min_interval_secs: u64,
    default_timezone: String,
}

impl ScheduleEditTool {
    pub fn new(
        store: Arc<dyn SchedulerStore>,
        min_interval_secs: u64,
        default_timezone: String,
    ) -> Self {
        Self {
            store,
            min_interval_secs,
            default_timezone,
        }
    }
}

#[async_trait]
impl Tool for ScheduleEditTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_EDIT_TOOL_NAME,
            Some(
                "Edit an existing scheduled task. You can update the name, goal, cadence, \
                 timezone, notification policy, or status (active/paused). If changing the \
                 cadence, provide both cadence_type and cadence_value."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["schedule_id"],
                "properties": {
                    "schedule_id": {
                        "type": "string",
                        "description": "ID of the schedule to edit",
                        "minLength": 1
                    },
                    "name": {
                        "type": "string",
                        "description": "New name for the schedule"
                    },
                    "goal": {
                        "type": "string",
                        "description": "New goal/prompt for the scheduled task"
                    },
                    "cadence_type": {
                        "type": "string",
                        "enum": ["cron", "once", "interval"],
                        "description": "New cadence type (must also provide cadence_value)"
                    },
                    "cadence_value": {
                        "type": "string",
                        "description": "New cadence value (must also provide cadence_type)"
                    },
                    "timezone": {
                        "type": "string",
                        "description": "New IANA timezone for cron schedules"
                    },
                    "notification": {
                        "type": "string",
                        "enum": ["always", "conditional", "never"],
                        "description": "New notification policy"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["active", "paused"],
                        "description": "Set status to active (resume) or paused"
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleEditArgs = parse_args(SCHEDULE_EDIT_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_EDIT_TOOL_NAME)?;

        // Fetch existing schedule and check ownership.
        let existing = self
            .store
            .get_schedule(&request.schedule_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))?;

        if existing.user_id != user_id {
            return Err(execution_failed(
                SCHEDULE_EDIT_TOOL_NAME,
                format!("schedule {} does not belong to you", request.schedule_id),
            ));
        }

        let mut patch = SchedulePatch::default();

        // Name
        if let Some(ref name) = request.name {
            patch.name = Some(Some(name.clone()));
        }

        // Goal
        if let Some(ref goal) = request.goal {
            patch.goal = Some(goal.clone());
        }

        // Notification policy
        if let Some(ref n) = request.notification {
            patch.notification_policy =
                Some(parse_notification_policy(n, SCHEDULE_EDIT_TOOL_NAME)?);
        }

        // Cadence changes
        let new_cadence = match (
            request.cadence_type.as_deref(),
            request.cadence_value.as_deref(),
        ) {
            (Some(ct), Some(cv)) => {
                let tz = request
                    .timezone
                    .as_deref()
                    .unwrap_or(&self.default_timezone);
                let cadence = parse_cadence(ct, cv, tz)
                    .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))?;
                validate_cadence(&cadence, self.min_interval_secs)
                    .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))?;
                Some(cadence)
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(invalid_args(
                    SCHEDULE_EDIT_TOOL_NAME,
                    "cadence_type and cadence_value must both be provided when changing cadence",
                ));
            }
            (None, None) => {
                // If only timezone changes for an existing cron cadence, rebuild it.
                if let Some(ref tz) = request.timezone {
                    if let ScheduleCadence::Cron { ref expression, .. } = existing.cadence {
                        let cadence = parse_cadence("cron", expression, tz).map_err(|e| {
                            execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string())
                        })?;
                        validate_cadence(&cadence, self.min_interval_secs).map_err(|e| {
                            execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string())
                        })?;
                        Some(cadence)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
        };

        // Determine the effective cadence for next_run_at recomputation.
        let effective_cadence = new_cadence.as_ref().unwrap_or(&existing.cadence);

        if let Some(ref cadence) = new_cadence {
            patch.cadence = Some(cadence.clone());
        }

        // Status changes
        let new_status = request
            .status
            .as_deref()
            .map(|s| parse_schedule_status(s, SCHEDULE_EDIT_TOOL_NAME))
            .transpose()?;

        if let Some(status) = new_status {
            patch.status = Some(status);
        }

        // Recompute next_run_at based on status and cadence changes.
        let effective_status = new_status.unwrap_or(existing.status);
        match effective_status {
            ScheduleStatus::Paused => {
                patch.next_run_at = Some(None);
            }
            ScheduleStatus::Active => {
                if new_cadence.is_some() || new_status.is_some() {
                    let next_run = next_run_for_cadence(effective_cadence, Utc::now())
                        .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))?;
                    patch.next_run_at = Some(next_run.map(|dt| dt.to_rfc3339()));
                }
            }
            _ => {}
        }

        patch.updated_at = Some(Utc::now().to_rfc3339());

        let updated = self
            .store
            .update_schedule(&request.schedule_id, &patch)
            .await
            .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))?;

        tracing::info!(
            tool = SCHEDULE_EDIT_TOOL_NAME,
            schedule_id = %request.schedule_id,
            "schedule updated"
        );

        let result = schedule_to_json(&updated);
        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_EDIT_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// ScheduleDeleteTool
// ---------------------------------------------------------------------------

pub struct ScheduleDeleteTool {
    store: Arc<dyn SchedulerStore>,
}

impl ScheduleDeleteTool {
    pub fn new(store: Arc<dyn SchedulerStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ScheduleDeleteTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_DELETE_TOOL_NAME,
            Some(
                "Delete a scheduled task permanently. Use schedule_search to find the schedule_id \
                 first. This action cannot be undone."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["schedule_id"],
                "properties": {
                    "schedule_id": {
                        "type": "string",
                        "description": "ID of the schedule to delete",
                        "minLength": 1
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleDeleteArgs = parse_args(SCHEDULE_DELETE_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_DELETE_TOOL_NAME)?;

        // Fetch existing schedule and check ownership.
        let existing = self
            .store
            .get_schedule(&request.schedule_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_DELETE_TOOL_NAME, e.to_string()))?;

        if existing.user_id != user_id {
            return Err(execution_failed(
                SCHEDULE_DELETE_TOOL_NAME,
                format!("schedule {} does not belong to you", request.schedule_id),
            ));
        }

        let deleted = self
            .store
            .delete_schedule(&request.schedule_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_DELETE_TOOL_NAME, e.to_string()))?;

        if !deleted {
            return Err(execution_failed(
                SCHEDULE_DELETE_TOOL_NAME,
                format!("schedule {} not found", request.schedule_id),
            ));
        }

        tracing::info!(
            tool = SCHEDULE_DELETE_TOOL_NAME,
            schedule_id = %request.schedule_id,
            "schedule deleted"
        );

        let result = json!({
            "schedule_id": request.schedule_id,
            "message": "Schedule deleted successfully."
        });
        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_DELETE_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::SideEffecting
    }
}

// ---------------------------------------------------------------------------
// ScheduleRunsTool
// ---------------------------------------------------------------------------

pub struct ScheduleRunsTool {
    store: Arc<dyn SchedulerStore>,
}

impl ScheduleRunsTool {
    pub fn new(store: Arc<dyn SchedulerStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ScheduleRunsTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_RUNS_TOOL_NAME,
            Some(
                "List recent run history for a scheduled task. Returns metadata per run \
                 (timestamps, status, summary) without full output. Use schedule_run_output \
                 to retrieve the complete output of a specific run."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["schedule_id"],
                "properties": {
                    "schedule_id": {
                        "type": "string",
                        "description": "ID of the schedule to list runs for",
                        "minLength": 1
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max runs to return (default 5, max 20)",
                        "minimum": 1,
                        "maximum": 20,
                        "default": 5
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleRunsArgs = parse_args(SCHEDULE_RUNS_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_RUNS_TOOL_NAME)?;

        // Verify ownership.
        let schedule = self
            .store
            .get_schedule(&request.schedule_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_RUNS_TOOL_NAME, e.to_string()))?;

        if schedule.user_id != user_id {
            return Err(execution_failed(
                SCHEDULE_RUNS_TOOL_NAME,
                format!("schedule {} does not belong to you", request.schedule_id),
            ));
        }

        let limit = request.limit.unwrap_or(5).min(20);
        let runs = self
            .store
            .get_run_history(&request.schedule_id, limit)
            .await
            .map_err(|e| execution_failed(SCHEDULE_RUNS_TOOL_NAME, e.to_string()))?;

        let run_entries: Vec<serde_json::Value> = runs
            .iter()
            .map(|r| {
                json!({
                    "run_id": r.run_id,
                    "started_at": r.started_at,
                    "finished_at": r.finished_at,
                    "status": r.status,
                    "output_summary": r.output_summary,
                    "turn_count": r.turn_count,
                    "cost": r.cost,
                    "notified": r.notified,
                })
            })
            .collect();

        let result = json!({
            "schedule_id": request.schedule_id,
            "runs": run_entries,
            "count": run_entries.len(),
        });

        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_RUNS_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// ScheduleRunOutputTool
// ---------------------------------------------------------------------------

pub struct ScheduleRunOutputTool {
    store: Arc<dyn SchedulerStore>,
}

impl ScheduleRunOutputTool {
    pub fn new(store: Arc<dyn SchedulerStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl Tool for ScheduleRunOutputTool {
    fn schema(&self) -> FunctionDecl {
        FunctionDecl::new(
            SCHEDULE_RUN_OUTPUT_TOOL_NAME,
            Some(
                "Retrieve the full output of a specific schedule run, with optional chunking \
                 to avoid truncation. Use schedule_runs first to get the run_id."
                    .to_owned(),
            ),
            json!({
                "type": "object",
                "required": ["run_id"],
                "properties": {
                    "run_id": {
                        "type": "string",
                        "description": "ID of the run to get output for",
                        "minLength": 1
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Character offset to start from (default 0)",
                        "minimum": 0,
                        "default": 0
                    },
                    "max_chars": {
                        "type": "integer",
                        "description": "Maximum characters to return (default 4000, max 8000)",
                        "minimum": 200,
                        "maximum": 8000,
                        "default": 4000
                    }
                }
            }),
        )
    }

    async fn execute(
        &self,
        args: &str,
        context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let request: ScheduleRunOutputArgs = parse_args(SCHEDULE_RUN_OUTPUT_TOOL_NAME, args)?;
        let user_id = resolve_user_id(context, SCHEDULE_RUN_OUTPUT_TOOL_NAME)?;

        let run = self
            .store
            .get_run_by_id(&request.run_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_RUN_OUTPUT_TOOL_NAME, e.to_string()))?;

        // Verify ownership through the schedule.
        let schedule = self
            .store
            .get_schedule(&run.schedule_id)
            .await
            .map_err(|e| execution_failed(SCHEDULE_RUN_OUTPUT_TOOL_NAME, e.to_string()))?;

        if schedule.user_id != user_id {
            return Err(execution_failed(
                SCHEDULE_RUN_OUTPUT_TOOL_NAME,
                format!("schedule {} does not belong to you", run.schedule_id),
            ));
        }

        let Some(ref full_output) = run.output else {
            let result = json!({
                "run_id": request.run_id,
                "message": "No output stored for this run.",
            });
            return serde_json::to_string(&result)
                .map_err(|e| execution_failed(SCHEDULE_RUN_OUTPUT_TOOL_NAME, e.to_string()));
        };

        let offset = request.offset.unwrap_or(0);
        let max_chars = request.max_chars.unwrap_or(4000).clamp(200, 8000);
        let total_len = full_output.len();

        if offset >= total_len {
            let result = json!({
                "run_id": request.run_id,
                "offset": offset,
                "max_chars": max_chars,
                "chunk": "",
                "remaining": 0,
                "total_length": total_len,
            });
            return serde_json::to_string(&result)
                .map_err(|e| execution_failed(SCHEDULE_RUN_OUTPUT_TOOL_NAME, e.to_string()));
        }

        let end = (offset + max_chars).min(total_len);
        // Ensure we don't split a multi-byte char.
        let safe_end = if full_output.is_char_boundary(end) {
            end
        } else {
            let mut e = end;
            while e > offset && !full_output.is_char_boundary(e) {
                e -= 1;
            }
            e
        };

        let chunk = &full_output[offset..safe_end];
        let remaining = total_len.saturating_sub(safe_end);

        let result = json!({
            "run_id": request.run_id,
            "offset": offset,
            "max_chars": max_chars,
            "chunk": chunk,
            "remaining": remaining,
            "total_length": total_len,
        });

        serde_json::to_string(&result)
            .map_err(|e| execution_failed(SCHEDULE_RUN_OUTPUT_TOOL_NAME, e.to_string()))
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(10)
    }

    fn safety_tier(&self) -> SafetyTier {
        SafetyTier::ReadOnly
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_scheduler_tools(
    registry: &mut crate::ToolRegistry,
    store: Arc<dyn SchedulerStore>,
    config: &SchedulerConfig,
) {
    registry.register(
        SCHEDULE_CREATE_TOOL_NAME,
        ScheduleCreateTool::new(
            store.clone(),
            config.max_schedules_per_user,
            config.min_interval_secs,
            config.default_timezone.clone(),
        ),
    );
    registry.register(
        SCHEDULE_SEARCH_TOOL_NAME,
        ScheduleSearchTool::new(store.clone()),
    );
    registry.register(
        SCHEDULE_EDIT_TOOL_NAME,
        ScheduleEditTool::new(
            store.clone(),
            config.min_interval_secs,
            config.default_timezone.clone(),
        ),
    );
    registry.register(SCHEDULE_DELETE_TOOL_NAME, ScheduleDeleteTool::new(store.clone()));
    registry.register(
        SCHEDULE_RUNS_TOOL_NAME,
        ScheduleRunsTool::new(store.clone()),
    );
    registry.register(
        SCHEDULE_RUN_OUTPUT_TOOL_NAME,
        ScheduleRunOutputTool::new(store),
    );
}
