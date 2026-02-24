use async_trait::async_trait;
use libsql::{Connection, params};
use types::{
    NotificationPolicy, ScheduleCadence, ScheduleDefinition, SchedulePatch, ScheduleRunRecord,
    ScheduleRunStatus, ScheduleSearchFilters, ScheduleSearchResult, ScheduleStatus, SchedulerError,
};

fn store_error(message: String) -> SchedulerError {
    SchedulerError::Store { message }
}

#[async_trait]
pub trait SchedulerStore: Send + Sync {
    async fn create_schedule(&self, def: &ScheduleDefinition) -> Result<(), SchedulerError>;
    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleDefinition, SchedulerError>;
    async fn search_schedules(
        &self,
        user_id: &str,
        filters: &ScheduleSearchFilters,
    ) -> Result<ScheduleSearchResult, SchedulerError>;
    async fn count_schedules(&self, user_id: &str) -> Result<usize, SchedulerError>;
    async fn delete_schedule(&self, schedule_id: &str) -> Result<bool, SchedulerError>;
    async fn update_schedule(
        &self,
        schedule_id: &str,
        patch: &SchedulePatch,
    ) -> Result<ScheduleDefinition, SchedulerError>;
    async fn due_schedules(
        &self,
        now: &str,
        limit: usize,
    ) -> Result<Vec<ScheduleDefinition>, SchedulerError>;
    async fn record_run_and_reschedule(
        &self,
        schedule_id: &str,
        run: &ScheduleRunRecord,
        next_run_at: Option<String>,
        new_status: Option<ScheduleStatus>,
    ) -> Result<(), SchedulerError>;
    async fn prune_run_history(&self, schedule_id: &str, keep: usize)
    -> Result<(), SchedulerError>;
    async fn get_run_history(
        &self,
        schedule_id: &str,
        limit: usize,
    ) -> Result<Vec<ScheduleRunRecord>, SchedulerError>;
}

pub struct LibsqlSchedulerStore {
    conn: Connection,
}

impl LibsqlSchedulerStore {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }
}

#[async_trait]
impl SchedulerStore for LibsqlSchedulerStore {
    async fn create_schedule(&self, def: &ScheduleDefinition) -> Result<(), SchedulerError> {
        let cadence_json =
            serde_json::to_string(&def.cadence).map_err(|e| store_error(e.to_string()))?;
        let notification = notification_policy_to_str(def.notification_policy);
        let status = schedule_status_to_str(def.status);
        let last_run_status = def.last_run_status.map(run_status_to_str);

        self.conn
            .execute(
                "INSERT INTO schedules (
                    schedule_id, user_id, name, goal, cadence_json,
                    notification_policy, status, created_at, updated_at,
                    next_run_at, last_run_at, last_run_status, consecutive_failures
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    def.schedule_id.as_str(),
                    def.user_id.as_str(),
                    def.name.as_deref(),
                    def.goal.as_str(),
                    cadence_json,
                    notification,
                    status,
                    def.created_at.as_str(),
                    def.updated_at.as_str(),
                    def.next_run_at.as_deref(),
                    def.last_run_at.as_deref(),
                    last_run_status,
                    def.consecutive_failures
                ],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        Ok(())
    }

    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleDefinition, SchedulerError> {
        let mut rows = self
            .conn
            .query(
                "SELECT schedule_id, user_id, name, goal, cadence_json,
                        notification_policy, status, created_at, updated_at,
                        next_run_at, last_run_at, last_run_status, consecutive_failures
                 FROM schedules WHERE schedule_id = ?1",
                params![schedule_id],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        let row = rows
            .next()
            .await
            .map_err(|e| store_error(e.to_string()))?
            .ok_or_else(|| SchedulerError::NotFound {
                schedule_id: schedule_id.to_owned(),
            })?;

        schedule_from_row(&row)
    }

    async fn search_schedules(
        &self,
        user_id: &str,
        filters: &ScheduleSearchFilters,
    ) -> Result<ScheduleSearchResult, SchedulerError> {
        let limit = filters.limit.min(50);
        let offset = filters.offset;

        let mut where_clauses = vec!["user_id = ?1".to_owned()];
        let mut bind_values: Vec<libsql::Value> = vec![user_id.into()];
        let mut param_index = 2u32;

        if let Some(ref name_contains) = filters.name_contains {
            where_clauses.push(format!("name LIKE ?{param_index}"));
            bind_values.push(format!("%{name_contains}%").into());
            param_index += 1;
        }
        if let Some(status) = filters.status {
            where_clauses.push(format!("status = ?{param_index}"));
            bind_values.push(schedule_status_to_str(status).into());
            param_index += 1;
        }
        if let Some(ref cadence_type) = filters.cadence_type {
            where_clauses.push(format!(
                "json_extract(cadence_json, '$.type') = ?{param_index}"
            ));
            bind_values.push(cadence_type.clone().into());
            param_index += 1;
        }
        if let Some(notification_policy) = filters.notification_policy {
            where_clauses.push(format!("notification_policy = ?{param_index}"));
            bind_values.push(notification_policy_to_str(notification_policy).into());
            param_index += 1;
        }

        let where_sql = where_clauses.join(" AND ");

        // Count query
        let count_sql = format!("SELECT COUNT(*) FROM schedules WHERE {where_sql}");
        let mut count_rows = self
            .conn
            .query(
                &count_sql,
                libsql::params::Params::Positional(bind_values.clone()),
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;
        let count_row = count_rows
            .next()
            .await
            .map_err(|e| store_error(e.to_string()))?
            .ok_or_else(|| store_error("count query returned no rows".to_owned()))?;
        let total_count = count_row
            .get::<i64>(0)
            .map_err(|e| store_error(e.to_string()))? as usize;

        // Fetch query
        let limit_param = param_index;
        let offset_param = param_index + 1;
        bind_values.push((limit as i64).into());
        bind_values.push((offset as i64).into());

        let fetch_sql = format!(
            "SELECT schedule_id, user_id, name, goal, cadence_json,
                    notification_policy, status, created_at, updated_at,
                    next_run_at, last_run_at, last_run_status, consecutive_failures
             FROM schedules WHERE {where_sql}
             ORDER BY created_at DESC
             LIMIT ?{limit_param} OFFSET ?{offset_param}"
        );
        let mut rows = self
            .conn
            .query(&fetch_sql, libsql::params::Params::Positional(bind_values))
            .await
            .map_err(|e| store_error(e.to_string()))?;

        let mut schedules = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| store_error(e.to_string()))? {
            schedules.push(schedule_from_row(&row)?);
        }

        Ok(ScheduleSearchResult {
            schedules,
            total_count,
            offset,
            limit,
        })
    }

    async fn count_schedules(&self, user_id: &str) -> Result<usize, SchedulerError> {
        let mut rows = self
            .conn
            .query(
                "SELECT COUNT(*) FROM schedules WHERE user_id = ?1",
                params![user_id],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        let row = rows
            .next()
            .await
            .map_err(|e| store_error(e.to_string()))?
            .ok_or_else(|| store_error("count query returned no rows".to_owned()))?;

        let count = row.get::<i64>(0).map_err(|e| store_error(e.to_string()))? as usize;
        Ok(count)
    }

    async fn delete_schedule(&self, schedule_id: &str) -> Result<bool, SchedulerError> {
        let affected = self
            .conn
            .execute(
                "DELETE FROM schedules WHERE schedule_id = ?1",
                params![schedule_id],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;
        Ok(affected > 0)
    }

    async fn update_schedule(
        &self,
        schedule_id: &str,
        patch: &SchedulePatch,
    ) -> Result<ScheduleDefinition, SchedulerError> {
        let mut set_clauses = Vec::new();
        let mut bind_values: Vec<libsql::Value> = Vec::new();
        let mut param_index = 1u32;

        if let Some(ref name) = patch.name {
            set_clauses.push(format!("name = ?{param_index}"));
            bind_values.push(
                name.as_deref()
                    .map(|s| libsql::Value::from(s.to_owned()))
                    .unwrap_or(libsql::Value::Null),
            );
            param_index += 1;
        }
        if let Some(ref goal) = patch.goal {
            set_clauses.push(format!("goal = ?{param_index}"));
            bind_values.push(goal.clone().into());
            param_index += 1;
        }
        if let Some(ref cadence) = patch.cadence {
            let cadence_json =
                serde_json::to_string(cadence).map_err(|e| store_error(e.to_string()))?;
            set_clauses.push(format!("cadence_json = ?{param_index}"));
            bind_values.push(cadence_json.into());
            param_index += 1;
        }
        if let Some(notification_policy) = patch.notification_policy {
            set_clauses.push(format!("notification_policy = ?{param_index}"));
            bind_values.push(notification_policy_to_str(notification_policy).into());
            param_index += 1;
        }
        if let Some(status) = patch.status {
            set_clauses.push(format!("status = ?{param_index}"));
            bind_values.push(schedule_status_to_str(status).into());
            param_index += 1;
        }
        if let Some(ref next_run_at) = patch.next_run_at {
            set_clauses.push(format!("next_run_at = ?{param_index}"));
            bind_values.push(
                next_run_at
                    .as_deref()
                    .map(|s| libsql::Value::from(s.to_owned()))
                    .unwrap_or(libsql::Value::Null),
            );
            param_index += 1;
        }
        if let Some(consecutive_failures) = patch.consecutive_failures {
            set_clauses.push(format!("consecutive_failures = ?{param_index}"));
            bind_values.push((consecutive_failures as i64).into());
            param_index += 1;
        }

        // Always update updated_at
        let updated_at = patch
            .updated_at
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
        set_clauses.push(format!("updated_at = ?{param_index}"));
        bind_values.push(updated_at.into());
        param_index += 1;

        if set_clauses.is_empty() {
            return self.get_schedule(schedule_id).await;
        }

        let set_sql = set_clauses.join(", ");
        let sql = format!("UPDATE schedules SET {set_sql} WHERE schedule_id = ?{param_index}");
        bind_values.push(schedule_id.into());

        let affected = self
            .conn
            .execute(&sql, libsql::params::Params::Positional(bind_values))
            .await
            .map_err(|e| store_error(e.to_string()))?;

        if affected == 0 {
            return Err(SchedulerError::NotFound {
                schedule_id: schedule_id.to_owned(),
            });
        }

        self.get_schedule(schedule_id).await
    }

    async fn due_schedules(
        &self,
        now: &str,
        limit: usize,
    ) -> Result<Vec<ScheduleDefinition>, SchedulerError> {
        let mut rows = self
            .conn
            .query(
                "SELECT schedule_id, user_id, name, goal, cadence_json,
                        notification_policy, status, created_at, updated_at,
                        next_run_at, last_run_at, last_run_status, consecutive_failures
                 FROM schedules
                 WHERE status = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ?1
                 ORDER BY next_run_at ASC
                 LIMIT ?2",
                params![now, limit as i64],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        let mut schedules = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| store_error(e.to_string()))? {
            schedules.push(schedule_from_row(&row)?);
        }
        Ok(schedules)
    }

    async fn record_run_and_reschedule(
        &self,
        schedule_id: &str,
        run: &ScheduleRunRecord,
        next_run_at: Option<String>,
        new_status: Option<ScheduleStatus>,
    ) -> Result<(), SchedulerError> {
        let run_status = run_status_to_str(run.status);

        // Insert run record
        self.conn
            .execute(
                "INSERT INTO schedule_runs (
                    run_id, schedule_id, started_at, finished_at,
                    status, output_summary, turn_count, cost, notified
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    run.run_id.as_str(),
                    run.schedule_id.as_str(),
                    run.started_at.as_str(),
                    run.finished_at.as_str(),
                    run_status,
                    run.output_summary.as_deref(),
                    run.turn_count,
                    run.cost,
                    run.notified
                ],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        // Compute consecutive_failures update
        let consecutive_failures_expr = match run.status {
            ScheduleRunStatus::Failed => "consecutive_failures + 1",
            _ => "0",
        };

        // Update schedule
        let status_sql = match new_status {
            Some(s) => format!("status = '{}'", schedule_status_to_str(s)),
            None => "status = status".to_owned(),
        };
        let next_run_sql = match &next_run_at {
            Some(_) => "next_run_at = ?4".to_owned(),
            None => "next_run_at = NULL".to_owned(),
        };

        let sql = format!(
            "UPDATE schedules SET
                last_run_at = ?1,
                last_run_status = ?2,
                consecutive_failures = {consecutive_failures_expr},
                {next_run_sql},
                {status_sql},
                updated_at = ?3
             WHERE schedule_id = ?5"
        );

        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                &sql,
                params![
                    run.finished_at.as_str(),
                    run_status,
                    now,
                    next_run_at.as_deref(),
                    schedule_id
                ],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        Ok(())
    }

    async fn prune_run_history(
        &self,
        schedule_id: &str,
        keep: usize,
    ) -> Result<(), SchedulerError> {
        self.conn
            .execute(
                "DELETE FROM schedule_runs
                 WHERE schedule_id = ?1
                   AND run_id NOT IN (
                       SELECT run_id FROM schedule_runs
                       WHERE schedule_id = ?1
                       ORDER BY started_at DESC
                       LIMIT ?2
                   )",
                params![schedule_id, keep as i64],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;
        Ok(())
    }

    async fn get_run_history(
        &self,
        schedule_id: &str,
        limit: usize,
    ) -> Result<Vec<ScheduleRunRecord>, SchedulerError> {
        let mut rows = self
            .conn
            .query(
                "SELECT run_id, schedule_id, started_at, finished_at,
                        status, output_summary, turn_count, cost, notified
                 FROM schedule_runs
                 WHERE schedule_id = ?1
                 ORDER BY started_at DESC
                 LIMIT ?2",
                params![schedule_id, limit as i64],
            )
            .await
            .map_err(|e| store_error(e.to_string()))?;

        let mut records = Vec::new();
        while let Some(row) = rows.next().await.map_err(|e| store_error(e.to_string()))? {
            records.push(run_record_from_row(&row)?);
        }
        Ok(records)
    }
}

fn schedule_from_row(row: &libsql::Row) -> Result<ScheduleDefinition, SchedulerError> {
    let schedule_id: String = row.get(0).map_err(|e| store_error(e.to_string()))?;
    let user_id: String = row.get(1).map_err(|e| store_error(e.to_string()))?;
    let name: Option<String> = row.get(2).map_err(|e| store_error(e.to_string()))?;
    let goal: String = row.get(3).map_err(|e| store_error(e.to_string()))?;
    let cadence_json: String = row.get(4).map_err(|e| store_error(e.to_string()))?;
    let notification_str: String = row.get(5).map_err(|e| store_error(e.to_string()))?;
    let status_str: String = row.get(6).map_err(|e| store_error(e.to_string()))?;
    let created_at: String = row.get(7).map_err(|e| store_error(e.to_string()))?;
    let updated_at: String = row.get(8).map_err(|e| store_error(e.to_string()))?;
    let next_run_at: Option<String> = row.get(9).map_err(|e| store_error(e.to_string()))?;
    let last_run_at: Option<String> = row.get(10).map_err(|e| store_error(e.to_string()))?;
    let last_run_status_str: Option<String> =
        row.get(11).map_err(|e| store_error(e.to_string()))?;
    let consecutive_failures: i64 = row.get(12).map_err(|e| store_error(e.to_string()))?;

    let cadence: ScheduleCadence =
        serde_json::from_str(&cadence_json).map_err(|e| store_error(e.to_string()))?;
    let notification_policy = notification_policy_from_str(&notification_str)?;
    let status = schedule_status_from_str(&status_str)?;
    let last_run_status = last_run_status_str
        .as_deref()
        .map(run_status_from_str)
        .transpose()?;

    Ok(ScheduleDefinition {
        schedule_id,
        user_id,
        name,
        goal,
        cadence,
        notification_policy,
        status,
        created_at,
        updated_at,
        next_run_at,
        last_run_at,
        last_run_status,
        consecutive_failures: consecutive_failures as u32,
    })
}

fn run_record_from_row(row: &libsql::Row) -> Result<ScheduleRunRecord, SchedulerError> {
    let run_id: String = row.get(0).map_err(|e| store_error(e.to_string()))?;
    let schedule_id: String = row.get(1).map_err(|e| store_error(e.to_string()))?;
    let started_at: String = row.get(2).map_err(|e| store_error(e.to_string()))?;
    let finished_at: String = row.get(3).map_err(|e| store_error(e.to_string()))?;
    let status_str: String = row.get(4).map_err(|e| store_error(e.to_string()))?;
    let output_summary: Option<String> = row.get(5).map_err(|e| store_error(e.to_string()))?;
    let turn_count: i64 = row.get(6).map_err(|e| store_error(e.to_string()))?;
    let cost: f64 = row.get(7).map_err(|e| store_error(e.to_string()))?;
    let notified: bool = row.get(8).map_err(|e| store_error(e.to_string()))?;

    let status = run_status_from_str(&status_str)?;

    Ok(ScheduleRunRecord {
        run_id,
        schedule_id,
        started_at,
        finished_at,
        status,
        output_summary,
        turn_count: turn_count as u32,
        cost,
        notified,
    })
}

fn notification_policy_to_str(policy: NotificationPolicy) -> &'static str {
    match policy {
        NotificationPolicy::Always => "always",
        NotificationPolicy::Conditional => "conditional",
        NotificationPolicy::Never => "never",
    }
}

fn notification_policy_from_str(s: &str) -> Result<NotificationPolicy, SchedulerError> {
    match s {
        "always" => Ok(NotificationPolicy::Always),
        "conditional" => Ok(NotificationPolicy::Conditional),
        "never" => Ok(NotificationPolicy::Never),
        _ => Err(store_error(format!("unknown notification policy: {s}"))),
    }
}

fn schedule_status_to_str(status: ScheduleStatus) -> &'static str {
    match status {
        ScheduleStatus::Active => "active",
        ScheduleStatus::Paused => "paused",
        ScheduleStatus::Completed => "completed",
        ScheduleStatus::Disabled => "disabled",
    }
}

fn schedule_status_from_str(s: &str) -> Result<ScheduleStatus, SchedulerError> {
    match s {
        "active" => Ok(ScheduleStatus::Active),
        "paused" => Ok(ScheduleStatus::Paused),
        "completed" => Ok(ScheduleStatus::Completed),
        "disabled" => Ok(ScheduleStatus::Disabled),
        _ => Err(store_error(format!("unknown schedule status: {s}"))),
    }
}

fn run_status_to_str(status: ScheduleRunStatus) -> &'static str {
    match status {
        ScheduleRunStatus::Success => "success",
        ScheduleRunStatus::Failed => "failed",
        ScheduleRunStatus::Cancelled => "cancelled",
    }
}

fn run_status_from_str(s: &str) -> Result<ScheduleRunStatus, SchedulerError> {
    match s {
        "success" => Ok(ScheduleRunStatus::Success),
        "failed" => Ok(ScheduleRunStatus::Failed),
        "cancelled" => Ok(ScheduleRunStatus::Cancelled),
        _ => Err(store_error(format!("unknown run status: {s}"))),
    }
}
