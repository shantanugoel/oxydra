# Phase 19: Scheduler System — Implementation Plan

## Executive Summary

Phase 19 adds a durable, governed scheduler that lets the LLM (or operator) create one-off and periodic tasks. When a schedule fires, it dispatches a bounded agent turn through the same `AgentRuntime.run_session` pipeline used for interactive turns — inheriting all policy controls (budgets, tool gating, cancellation, scrubbing, audit). Notification routing is conditional: always, conditionally (LLM decides), or never.

---

## Research Takeaways from Comparable Projects

| Project | Key Patterns Adopted | Key Patterns Avoided |
|---------|---------------------|---------------------|
| **zeroclaw** | SQLite-backed durable store, separate `cron_jobs` + `cron_runs` tables, `due_jobs()` poll query, `Schedule` enum (Cron/At/Every), delivery config, auto-delete one-shot, truncated output, security policy enforcement at creation time, retry with backoff | Synchronous rusqlite (Oxydra uses async libsql), shell-job focus |
| **microclaw** | Per-tool CRUD (schedule_task, list/pause/resume/cancel), chat-scoped schedules, DLQ for failed runs, run history logs, timezone support | Separate tool per action (Oxydra prefers fewer tools with richer schemas) |

**Design decisions influenced by research:**

1. **Prompt-based agent jobs, not shell commands** — Oxydra schedules dispatch LLM agent turns with a prompt. There are no shell-command jobs; the LLM uses its existing tools (including shell_exec) to accomplish the goal.
2. **Same DB, separate tables** — Use the existing libsql memory database with new migration-managed tables, not a separate SQLite file.
3. **Polling executor** — A simple tokio interval loop (like zeroclaw) rather than a complex event-driven system. Sufficient for the expected cadence and avoids complexity.
4. **DLQ-like run history** — Track execution history per schedule for observability (inspired by microclaw).

---

## Step-by-Step Implementation Plan

### Step 1: Types — Schedule Domain Types in `crates/types`

**File: `crates/types/src/scheduler.rs`** (new)

Define the core scheduler domain types:

```rust
// --- Schedule cadence ---

/// How/when a schedule fires.
pub enum ScheduleCadence {
    /// Fire once at a specific UTC timestamp.
    Once { at: DateTime<Utc> },
    /// Fire periodically according to a cron expression (5-field standard).
    Cron { expression: String, timezone: String },
    /// Fire at a fixed interval (minimum 60s).
    Interval { every_secs: u64 },
}

// --- Notification policy ---

/// How the scheduler routes output after a scheduled turn completes.
pub enum NotificationPolicy {
    /// Always send the response to the user's active channel.
    Always,
    /// The LLM response is inspected for a notification signal.
    /// If the response contains the [NOTIFY] marker, it is routed; otherwise silent.
    Conditional,
    /// Never notify — results stored in memory only.
    Never,
}

// --- Schedule status ---

pub enum ScheduleStatus {
    Active,
    Paused,
    Completed,   // One-shot that has fired
    Disabled,    // Auto-disabled after consecutive failures
}

// --- Schedule definition (persisted) ---

pub struct ScheduleDefinition {
    pub schedule_id: String,           // UUID
    pub user_id: String,               // owning user
    pub name: Option<String>,          // human-readable label
    pub goal: String,                  // the prompt sent to the LLM
    pub cadence: ScheduleCadence,
    pub notification_policy: NotificationPolicy,
    pub status: ScheduleStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub next_run_at: Option<DateTime<Utc>>,  // None = completed one-shot or paused
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_run_status: Option<ScheduleRunStatus>,
    pub consecutive_failures: u32,
}

pub enum ScheduleRunStatus { Success, Failed, Cancelled }

// --- Execution record (audit trail) ---

pub struct ScheduleRunRecord {
    pub run_id: String,
    pub schedule_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: ScheduleRunStatus,
    pub output_summary: Option<String>,  // truncated LLM response
    pub turn_count: usize,
    pub cost: f64,
    pub notified: bool,
}
```

**Why these fields matter for LLM interaction:**

- `goal` is the raw prompt that gets sent as the user message to `AgentRuntime.run_session`. It must be a complete, self-contained instruction because there is no interactive follow-up.
- `notification_policy: Conditional` requires the LLM to output a structured signal. We define a convention: if the LLM's final response contains `[NOTIFY]` at the start, the response is routed. Otherwise it's silent. This is injected into the system prompt prefix for scheduled turns.
- `max_turns` / `max_cost` are **not** stored per-schedule. They are global operator-configured values in `SchedulerConfig`. The LLM has no control over execution budgets for scheduled runs.

**LLM schema design consideration:** The `goal` field is free-form natural language. We do NOT ask the LLM to produce structured cadence specs — instead, the `schedule_create` tool accepts clearly typed parameters that *our code* validates and converts to `ScheduleCadence`. This prevents misparse issues where LLMs produce invalid cron expressions.

**Wire to `crates/types/src/lib.rs`:** Re-export the module.

---

### Step 2: Error Variants

**File: `crates/types/src/error.rs`**

Add a new `SchedulerError` enum:

```rust
pub enum SchedulerError {
    #[error("invalid cron expression: {expression}: {reason}")]
    InvalidCronExpression { expression: String, reason: String },
    #[error("invalid schedule cadence: {reason}")]
    InvalidCadence { reason: String },
    #[error("schedule not found: {schedule_id}")]
    NotFound { schedule_id: String },
    #[error("schedule store error: {message}")]
    Store { message: String },
    #[error("schedule execution failed: {message}")]
    Execution { message: String },
    #[error("user {user_id} has reached the maximum number of schedules ({max})")]
    LimitExceeded { user_id: String, max: usize },
    #[error("unauthorized: schedule {schedule_id} does not belong to user {user_id}")]
    Unauthorized { schedule_id: String, user_id: String },
}
```

Add `Scheduler(SchedulerError)` variant to `RuntimeError`.

---

### Step 3: Configuration

**File: `crates/types/src/config.rs`**

Add `SchedulerConfig` to `AgentConfig`:

```rust
pub struct SchedulerConfig {
    /// Whether scheduling is enabled. Default: false.
    pub enabled: bool,
    /// Polling interval in seconds for the executor. Default: 15.
    pub poll_interval_secs: u64,
    /// Maximum concurrent scheduled executions. Default: 2.
    pub max_concurrent: usize,
    /// Maximum schedules per user. Default: 50.
    pub max_schedules_per_user: usize,
    /// Max turns for each scheduled run (operator-only, not LLM-controlled). Default: 10.
    pub max_turns: usize,
    /// Max cost for each scheduled run (operator-only, not LLM-controlled). Default: 0.50.
    pub max_cost: f64,
    /// Maximum run history entries per schedule. Default: 20.
    pub max_run_history: usize,
    /// Minimum interval between runs in seconds (anti-abuse). Default: 60.
    pub min_interval_secs: u64,
    /// Default timezone for cron schedules. Default: "Asia/Kolkata".
    pub default_timezone: String,
    /// Consecutive failure count before auto-disabling a schedule. Default: 5.
    pub auto_disable_after_failures: u32,
}
```

Notes:
- `max_turns` and `max_cost` are **operator-configured only**. They do not appear in any tool schema. The LLM cannot override them. This prevents runaway scheduled executions.
- `default_timezone` is `Asia/Kolkata`. When a schedule is created without an explicit timezone, this applies.
- Scheduling is off by default — must be explicitly enabled.

---

### Step 4: Database Schema — Migrations in `crates/memory`

**Migration 0017: `crates/memory/migrations/0017_create_schedules_table.sql`**

```sql
CREATE TABLE IF NOT EXISTS schedules (
    schedule_id          TEXT PRIMARY KEY,
    user_id              TEXT NOT NULL,
    name                 TEXT,
    goal                 TEXT NOT NULL,
    cadence_json         TEXT NOT NULL,   -- JSON-serialized ScheduleCadence
    notification_policy  TEXT NOT NULL DEFAULT 'always',
    status               TEXT NOT NULL DEFAULT 'active',
    created_at           TEXT NOT NULL,
    updated_at           TEXT NOT NULL,
    next_run_at          TEXT,
    last_run_at          TEXT,
    last_run_status      TEXT,
    consecutive_failures INTEGER NOT NULL DEFAULT 0
);
```

**Migration 0018: `crates/memory/migrations/0018_create_schedules_indexes.sql`**

```sql
CREATE INDEX IF NOT EXISTS idx_schedules_user_id ON schedules(user_id);
CREATE INDEX IF NOT EXISTS idx_schedules_due ON schedules(next_run_at)
    WHERE status = 'active' AND next_run_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_schedules_status ON schedules(user_id, status);
CREATE INDEX IF NOT EXISTS idx_schedules_name ON schedules(user_id, name);
```

**Migration 0019: `crates/memory/migrations/0019_create_schedule_runs_table.sql`**

```sql
CREATE TABLE IF NOT EXISTS schedule_runs (
    run_id         TEXT PRIMARY KEY,
    schedule_id    TEXT NOT NULL,
    started_at     TEXT NOT NULL,
    finished_at    TEXT NOT NULL,
    status         TEXT NOT NULL,
    output_summary TEXT,
    turn_count     INTEGER NOT NULL DEFAULT 0,
    cost           REAL NOT NULL DEFAULT 0.0,
    notified       INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (schedule_id) REFERENCES schedules(schedule_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_schedule_runs_lookup
    ON schedule_runs(schedule_id, started_at DESC);
```

**Update `crates/memory/src/schema.rs`:** Add the three new migrations to the `MIGRATIONS` array and update `REQUIRED_TABLES` and `REQUIRED_INDEXES`.

---

### Step 5: SchedulerStore Trait & Implementation in `crates/memory`

**File: `crates/memory/src/scheduler_store.rs`** (new)

Define a trait (for testability) and implement it against libsql:

```rust
#[async_trait]
pub trait SchedulerStore: Send + Sync {
    /// Create a new schedule. Caller must validate limits beforehand.
    async fn create_schedule(&self, def: &ScheduleDefinition) -> Result<(), SchedulerError>;

    /// Get a single schedule by ID.
    async fn get_schedule(&self, schedule_id: &str) -> Result<ScheduleDefinition, SchedulerError>;

    /// Search schedules for a user with optional filters and pagination.
    async fn search_schedules(
        &self,
        user_id: &str,
        filters: &ScheduleSearchFilters,
    ) -> Result<ScheduleSearchResult, SchedulerError>;

    /// Count schedules for a user (for limit enforcement).
    async fn count_schedules(&self, user_id: &str) -> Result<usize, SchedulerError>;

    /// Delete a schedule (cascading deletes runs).
    async fn delete_schedule(&self, schedule_id: &str) -> Result<bool, SchedulerError>;

    /// Update mutable fields on a schedule. Only provided (Some) fields are changed.
    async fn update_schedule(
        &self,
        schedule_id: &str,
        patch: &SchedulePatch,
    ) -> Result<ScheduleDefinition, SchedulerError>;

    /// Query for schedules whose `next_run_at <= now` and `status = 'active'`.
    async fn due_schedules(
        &self,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<ScheduleDefinition>, SchedulerError>;

    /// After a run: update last_run_at, last_run_status, consecutive_failures,
    /// next_run_at (or mark completed if one-shot), and insert the run record.
    async fn record_run_and_reschedule(
        &self,
        schedule_id: &str,
        run: &ScheduleRunRecord,
        next_run_at: Option<DateTime<Utc>>,
        new_status: Option<ScheduleStatus>,
    ) -> Result<(), SchedulerError>;

    /// Prune old run records beyond the retention limit.
    async fn prune_run_history(
        &self,
        schedule_id: &str,
        keep: usize,
    ) -> Result<(), SchedulerError>;

    /// Get run history for a schedule (most recent first).
    async fn get_run_history(
        &self,
        schedule_id: &str,
        limit: usize,
    ) -> Result<Vec<ScheduleRunRecord>, SchedulerError>;
}
```

**Supporting types:**

```rust
/// Filters for schedule_search. All fields are optional.
/// If all None, returns all schedules for the user.
pub struct ScheduleSearchFilters {
    pub name_contains: Option<String>,
    pub status: Option<ScheduleStatus>,
    pub cadence_type: Option<String>,           // "cron", "once", "interval"
    pub notification_policy: Option<NotificationPolicy>,
    pub created_after: Option<DateTime<Utc>>,
    pub created_before: Option<DateTime<Utc>>,
    pub next_run_before: Option<DateTime<Utc>>,
    pub limit: usize,                            // default 20, max 50
    pub offset: usize,                           // for pagination
}

pub struct ScheduleSearchResult {
    pub schedules: Vec<ScheduleDefinition>,
    pub total_count: usize,   // total matching, for pagination hints
    pub offset: usize,
    pub limit: usize,
}

/// Patch for schedule_edit. Only Some fields are updated.
pub struct SchedulePatch {
    pub name: Option<Option<String>>,            // Some(None) clears name, Some(Some(..)) sets it
    pub goal: Option<String>,
    pub cadence: Option<ScheduleCadence>,
    pub notification_policy: Option<NotificationPolicy>,
    pub status: Option<ScheduleStatus>,          // Active/Paused for pause/resume
}
```

**Implementation in `LibsqlSchedulerStore`** (part of `crates/memory`):

- Uses the existing `LibsqlMemory`'s connection pool.
- All queries use parameterized SQL.
- `cadence_json` is `serde_json::to_string(&cadence)` for storage, deserialized on read.
- `due_schedules` uses the indexed `next_run_at` column:
  ```sql
  SELECT * FROM schedules
  WHERE status = 'active' AND next_run_at IS NOT NULL AND next_run_at <= ?1
  ORDER BY next_run_at ASC LIMIT ?2
  ```
- `search_schedules` builds a dynamic WHERE clause from filters, runs two queries (count + fetch) for pagination.
- `update_schedule` builds a dynamic SET clause from the non-None fields in `SchedulePatch`, always sets `updated_at`. Recomputes `next_run_at` when cadence or status changes.
- `record_run_and_reschedule` runs in a transaction: INSERT into `schedule_runs`, UPDATE `schedules`, then prune.

**Key interaction with scheduling:** The `next_run_at` computation happens in our Rust code (not in the LLM). We use the `cron` crate to parse 5-field expressions and compute the next occurrence. This is critical — the LLM never computes scheduling math.

---

### Step 6: Cadence Evaluation — Next-Run Computation

**File: `crates/memory/src/cadence.rs`** (new, or could be in `types` if needed by tools)

```rust
/// Compute the next run time for a given cadence after `after`.
/// Returns None for a one-shot that has already passed.
pub fn next_run_for_cadence(
    cadence: &ScheduleCadence,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, SchedulerError> {
    match cadence {
        ScheduleCadence::Once { at } => {
            if *at > after { Ok(Some(*at)) } else { Ok(None) }
        }
        ScheduleCadence::Cron { expression, timezone } => {
            let tz: chrono_tz::Tz = timezone.parse()
                .map_err(|_| SchedulerError::InvalidCadence {
                    reason: format!("invalid timezone: {timezone}"),
                })?;
            let schedule = cron::Schedule::from_str(expression)
                .map_err(|e| SchedulerError::InvalidCronExpression {
                    expression: expression.clone(),
                    reason: e.to_string(),
                })?;
            let next = schedule
                .after(&after.with_timezone(&tz))
                .next();
            Ok(next.map(|dt| dt.with_timezone(&Utc)))
        }
        ScheduleCadence::Interval { every_secs } => {
            Ok(Some(after + chrono::Duration::seconds(*every_secs as i64)))
        }
    }
}

/// Validate a cadence at creation/edit time.
pub fn validate_cadence(
    cadence: &ScheduleCadence,
    min_interval_secs: u64,
) -> Result<(), SchedulerError> {
    match cadence {
        ScheduleCadence::Once { at } => {
            if *at <= Utc::now() {
                return Err(SchedulerError::InvalidCadence {
                    reason: "one-off time must be in the future".into(),
                });
            }
        }
        ScheduleCadence::Cron { expression, timezone } => {
            // Validate timezone
            let tz: chrono_tz::Tz = timezone.parse()
                .map_err(|_| SchedulerError::InvalidCadence {
                    reason: format!("invalid timezone: {timezone}"),
                })?;

            // Parse to validate expression
            let schedule = cron::Schedule::from_str(expression)
                .map_err(|e| SchedulerError::InvalidCronExpression {
                    expression: expression.clone(),
                    reason: e.to_string(),
                })?;

            // Check minimum interval between consecutive firings
            let now = Utc::now().with_timezone(&tz);
            let mut iter = schedule.after(&now);
            if let (Some(a), Some(b)) = (iter.next(), iter.next()) {
                let gap = (b - a).num_seconds();
                if gap < min_interval_secs as i64 {
                    return Err(SchedulerError::InvalidCadence {
                        reason: format!(
                            "cron fires every {gap}s, minimum is {min_interval_secs}s"
                        ),
                    });
                }
            }
        }
        ScheduleCadence::Interval { every_secs } => {
            if *every_secs < min_interval_secs {
                return Err(SchedulerError::InvalidCadence {
                    reason: format!(
                        "interval {every_secs}s is below minimum {min_interval_secs}s"
                    ),
                });
            }
        }
    }
    Ok(())
}
```

---

### Step 7: Scheduler Tools in `crates/tools`

**File: `crates/tools/src/scheduler_tools.rs`** (new)

Four tools exposed to the LLM:

#### 7a. `schedule_create`

```
Tool name: schedule_create
Safety tier: SideEffecting
Schema:
{
  "name": {
    "type": "string",
    "description": "Optional human-readable label for the schedule"
  },
  "goal": {
    "type": "string",
    "description": "Complete, self-contained instruction to execute. Write as if briefing
                    an agent with no prior context — it runs independently without
                    conversational history."
  },
  "cadence_type": {
    "type": "string",
    "enum": ["cron", "once", "interval"],
    "description": "Type of schedule: 'cron' (5-field expression), 'once' (ISO 8601 timestamp),
                    or 'interval' (seconds between runs, minimum 60)"
  },
  "cadence_value": {
    "type": "string",
    "description": "The cadence specification. For cron: '0 8 * * *' (5-field). For once:
                    '2026-03-01T09:00:00Z' (ISO 8601). For interval: '3600' (seconds)."
  },
  "timezone": {
    "type": "string",
    "description": "IANA timezone for cron schedules (e.g. 'Asia/Kolkata', 'US/Eastern').
                    Ignored for 'once' and 'interval'. Default: Asia/Kolkata."
  },
  "notification": {
    "type": "string",
    "enum": ["always", "conditional", "never"],
    "description": "When to notify. 'always': send every result. 'conditional': only if
                    result warrants attention. 'never': store silently. Default: 'always'."
  }
}
Required: ["goal", "cadence_type", "cadence_value"]
```

**What is NOT in the schema:**
- `max_turns` — operator-configured only via `SchedulerConfig.max_turns` (default 10). The LLM cannot override this.
- `max_cost` — operator-configured only via `SchedulerConfig.max_cost` (default 0.50). The LLM cannot override this.

**Critical LLM interaction design decisions:**

1. **`cadence_type` + `cadence_value` instead of a single freeform field.** This prevents the LLM from hallucinating a hybrid format. The `cadence_type` discriminant forces structured input that our code validates.

2. **`goal` is the full prompt.** The tool description explicitly instructs: *"Write the goal as a complete, self-contained instruction."* This is crucial because scheduled turns have no prior conversation history.

3. **`timezone` defaults to `Asia/Kolkata`** when not provided. This applies only to cron-type cadences. One-off timestamps are always UTC (ISO 8601 with offset). Interval cadences are timezone-agnostic.

4. **Validation at creation:** Our code validates the cron expression, checks the minimum interval, checks the per-user schedule limit, and computes `next_run_at` — all before persisting. The LLM gets a clear error message if anything is wrong.

5. **Return value:** Returns the created schedule's ID, name, next run time in both UTC and the schedule's timezone, so the LLM can confirm to the user in a human-friendly way.

**Execute implementation sketch:**

```rust
async fn execute(&self, args: Value, ctx: &ToolExecutionContext) -> Result<Value, ToolError> {
    let user_id = ctx.user_id();

    // 1. Check per-user schedule limit
    let count = self.store.count_schedules(user_id).await?;
    if count >= self.config.max_schedules_per_user {
        return Err(SchedulerError::LimitExceeded { ... }.into());
    }

    // 2. Parse & validate cadence
    let cadence = parse_cadence(
        args["cadence_type"].as_str()?,
        args["cadence_value"].as_str()?,
        args.get("timezone").and_then(|v| v.as_str())
            .unwrap_or(&self.config.default_timezone),
    )?;
    validate_cadence(&cadence, self.config.min_interval_secs)?;

    // 3. Compute first next_run_at
    let next_run = next_run_for_cadence(&cadence, Utc::now())?
        .ok_or_else(|| SchedulerError::InvalidCadence {
            reason: "schedule would never fire".into(),
        })?;

    // 4. Build and persist definition
    let def = ScheduleDefinition { ... };
    self.store.create_schedule(&def).await?;

    // 5. Return confirmation
    Ok(json!({
        "schedule_id": def.schedule_id,
        "name": def.name,
        "next_run_at": next_run.to_rfc3339(),
        "next_run_local": format_in_timezone(next_run, &cadence),
        "status": "active"
    }))
}
```

#### 7b. `schedule_search`

```
Tool name: schedule_search
Safety tier: ReadOnly
Schema:
{
  "name": {
    "type": "string",
    "description": "Filter by name (partial, case-insensitive match)"
  },
  "status": {
    "type": "string",
    "enum": ["active", "paused", "completed", "disabled"],
    "description": "Filter by status"
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
    "description": "Max results to return (default 20, max 50)"
  },
  "offset": {
    "type": "integer",
    "description": "Pagination offset (default 0). Use to fetch subsequent pages."
  }
}
Required: []  (all optional — no filters = list all)
```

**Return format:**

```json
{
  "schedules": [
    {
      "schedule_id": "sched-abc123",
      "name": "Weather check",
      "goal": "Check weather in SF and report...",  // truncated to 120 chars
      "cadence": "cron: 0 8 * * * (Asia/Kolkata)",
      "status": "active",
      "notification": "conditional",
      "next_run_at": "2026-02-25T02:30:00Z",
      "next_run_local": "2026-02-25 08:00:00 IST",
      "last_run_at": "2026-02-24T02:30:00Z",
      "last_run_status": "success"
    }
  ],
  "total": 37,
  "offset": 0,
  "limit": 20,
  "remaining": 17,
  "hint": "17 more results available. Use offset=20 to see the next page."
}
```

**LLM pagination guidance:** The `remaining` count and `hint` string tell the LLM exactly how many more results exist and what `offset` to use next. This is surfaced as a top-level field so the LLM naturally decides whether to paginate based on the user's request.

#### 7c. `schedule_edit`

```
Tool name: schedule_edit
Safety tier: SideEffecting
Schema:
{
  "schedule_id": {
    "type": "string",
    "description": "ID of the schedule to edit"
  },
  "name": {
    "type": "string",
    "description": "New name (set to empty string to clear)"
  },
  "goal": {
    "type": "string",
    "description": "New goal/prompt. Write as a complete, self-contained instruction."
  },
  "cadence_type": {
    "type": "string",
    "enum": ["cron", "once", "interval"],
    "description": "New cadence type (must also provide cadence_value)"
  },
  "cadence_value": {
    "type": "string",
    "description": "New cadence value (required if cadence_type is provided)"
  },
  "timezone": {
    "type": "string",
    "description": "New timezone for cron schedules"
  },
  "notification": {
    "type": "string",
    "enum": ["always", "conditional", "never"],
    "description": "New notification policy"
  },
  "status": {
    "type": "string",
    "enum": ["active", "paused"],
    "description": "Set to 'paused' to pause, 'active' to resume"
  }
}
Required: ["schedule_id"]  (at least one other field should be provided)
```

**Behavior details:**

- Only provided fields are modified; omitted fields are left unchanged.
- **Pause/resume:** Setting `status` to `"paused"` clears `next_run_at` (so the executor skips it). Setting `status` to `"active"` recomputes `next_run_at` from the current cadence.
- **Cadence change:** If `cadence_type` is provided, `cadence_value` must also be provided (validated). The new cadence is validated, and `next_run_at` is recomputed. If only `timezone` is changed (no cadence_type), only the timezone in the existing cron cadence is updated.
- **Goal change:** The new goal replaces the old one entirely. The tool description reminds the LLM to write complete instructions.
- **Ownership check:** Only the owning `user_id` can edit. Returns `SchedulerError::Unauthorized` otherwise.
- **Cannot edit completed/disabled schedules back to active without also providing a valid future cadence.** This prevents resurrecting stale one-shots.

**Return value:** The full updated `ScheduleDefinition` (same format as a single entry from `schedule_search`).

#### 7d. `schedule_delete`

```
Tool name: schedule_delete
Safety tier: SideEffecting
Schema:
{
  "schedule_id": {
    "type": "string",
    "description": "ID of the schedule to delete permanently"
  }
}
Required: ["schedule_id"]
```

Deletes the schedule. Cascading delete removes all run history. Ownership check enforced.

**Registration:** `register_scheduler_tools(registry, store, scheduler_config)` — similar pattern to `register_memory_tools`.

---

### Step 8: SchedulerExecutor in `crates/runtime`

**File: `crates/runtime/src/scheduler_executor.rs`** (new)

This is the background task that polls for due schedules and dispatches agent turns.

```rust
pub struct SchedulerExecutor {
    store: Arc<dyn SchedulerStore>,
    turn_runner: Arc<dyn GatewayTurnRunner>,
    config: SchedulerConfig,
    cancellation: CancellationToken,
}

impl SchedulerExecutor {
    pub async fn run(&self) {
        let mut interval = tokio::time::interval(
            Duration::from_secs(self.config.poll_interval_secs)
        );
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

    async fn tick(&self) {
        let due = match self.store
            .due_schedules(Utc::now(), self.config.max_concurrent)
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

        // Execute due schedules concurrently (bounded by max_concurrent)
        futures::stream::iter(due.into_iter().map(|s| self.execute_schedule(s)))
            .buffer_unordered(self.config.max_concurrent)
            .collect::<Vec<_>>()
            .await;
    }

    async fn execute_schedule(&self, schedule: ScheduleDefinition) {
        // ... see detailed flow below
    }
}
```

#### Detailed Execution Flow for `execute_schedule`:

```
1. Generate unique run_id (UUID).

2. Generate a schedule-specific runtime_session_id:
     format!("scheduled:{}", schedule.schedule_id)
   This gives each schedule its own session namespace in memory,
   so conversation history doesn't leak between schedules or
   interfere with the user's interactive sessions.

3. Build the scheduled prompt:
   - Start with the schedule's `goal` as the user message.
   - If notification_policy == Conditional, augment the system prompt with:
     "You are executing a scheduled task. If the result warrants notifying
      the user, begin your response with [NOTIFY] followed by the
      notification message. If no notification is needed, respond normally
      without [NOTIFY]."

4. Apply operator-configured budget limits (from SchedulerConfig):
   - max_turns = self.config.max_turns     (default 10)
   - max_cost  = self.config.max_cost      (default 0.50)
   The LLM has no control over these values.

5. Dispatch via GatewayTurnRunner.run_turn():
   - user_id: schedule.user_id
   - runtime_session_id: the schedule-specific session ID
   - prompt: the goal text
   - cancellation: child token of the executor's cancellation token
   - delta_sender: a channel we consume to capture the full response

6. Collect the full response text from the delta channel.

7. Record the run:
   - Create ScheduleRunRecord with timing, status, truncated output
     (output_summary is first 500 chars of response)
   - Compute next_run_at via next_run_for_cadence()
   - Determine new schedule status:
     - If one-shot (Once) and completed → status = Completed, next_run_at = None
     - If periodic and successful → reset consecutive_failures to 0
     - If periodic and failed → increment consecutive_failures
     - If consecutive_failures >= auto_disable_after_failures → status = Disabled
   - Call store.record_run_and_reschedule()

8. Handle notification:
   - If Always: route response to user's active channel via gateway
   - If Conditional: check for [NOTIFY] prefix in response
     - Present → extract message after [NOTIFY], route to channel
     - Absent → silent, only stored in memory
   - If Never: no routing

9. Handle errors:
   - Record the run with Failed status
   - Still compute and set next_run_at (so periodic schedules fire again)
   - Increment consecutive_failures
   - After N consecutive failures, auto-disable and record in run history
```

**Key interaction issues addressed:**

1. **Session isolation:** Each schedule gets its own `runtime_session_id` (`scheduled:{schedule_id}`). This means:
   - Scheduled runs don't pollute the user's interactive session history.
   - Each schedule accumulates its own conversation context across runs (useful for periodic tasks that should remember prior results).
   - Memory tools scoped to `user_id` still work correctly.

2. **Budget isolation:** Scheduled runs use operator-configured budget limits from `SchedulerConfig`. The executor creates a fresh `RuntimeLimits` per run. The LLM cannot override these — they are not exposed in any tool schema.

3. **Concurrency with interactive turns:** The `GatewayTurnRunner` trait already handles concurrent turns via its internal `contexts` map keyed by `runtime_session_id`. Since scheduled runs use distinct session IDs, they don't conflict with interactive turns. The only shared resource is the LLM provider — natural backpressure through the provider's rate limits handles this.

4. **LLM context for scheduled turns:** Scheduled turns start with a clean context (or restored from the schedule's session if memory is enabled). The system prompt is augmented with scheduler-specific instructions. The LLM has full tool access (same `ToolRegistry`) — it can search the web, read files, use memory tools, etc.

5. **Conditional notification protocol:** The `[NOTIFY]` marker is simple and robust. It avoids needing a structured JSON output schema (which would require tool-call-based notification, adding complexity). The marker is injected into the system prompt, so the LLM understands the contract. Our code does a simple `response.starts_with("[NOTIFY]")` check.

---

### Step 9: Notification Routing

**File: `crates/runtime/src/scheduler_executor.rs`** (part of the executor)

When notification is triggered, the executor needs to route the message to the user.

**Approach: Use the Gateway's broadcast mechanism.**

```rust
async fn notify_user(
    &self,
    user_id: &str,
    schedule: &ScheduleDefinition,
    message: &str,
) {
    self.gateway.publish_to_user(
        user_id,
        GatewayServerFrame::ScheduledNotification {
            schedule_id: schedule.schedule_id.clone(),
            schedule_name: schedule.name.clone(),
            message: message.to_owned(),
        },
    );
}
```

If the user has no active connection, the notification is stored in memory (it already is, via the scheduled session's conversation history) and can be surfaced when the user reconnects — but we don't implement a persistent push-notification queue in this phase. That's a future enhancement once external channels exist.

**New gateway frame:** Add `GatewayServerFrame::ScheduledNotification` variant to `crates/types/src/channel.rs`.

---

### Step 10: Gateway Integration — Spawning the Executor

**File: `crates/gateway/src/lib.rs`** (modified)

The `GatewayServer` gains an optional `SchedulerExecutor` that is spawned as a background tokio task when scheduling is enabled:

```rust
impl GatewayServer {
    pub fn with_scheduler(mut self, executor: SchedulerExecutor) -> Self {
        self.scheduler = Some(executor);
        self
    }

    pub async fn start(self: Arc<Self>) {
        // ... existing WebSocket server setup ...

        // Spawn scheduler if enabled
        if let Some(executor) = &self.scheduler {
            let executor = executor.clone();
            tokio::spawn(async move {
                executor.run().await;
            });
        }
    }
}
```

The executor's `CancellationToken` should be a child of the gateway's token so that gateway shutdown cleanly stops the scheduler.

---

### Step 11: Runner Bootstrap Wiring

**File: `crates/runner/src/main.rs`** (modified)

During startup:

1. Read `scheduler` config section from `AgentConfig`.
2. If `scheduler.enabled`:
   - Construct `LibsqlSchedulerStore` from the existing memory database connection.
   - Register scheduler tools (`schedule_create`, `schedule_search`, `schedule_edit`, `schedule_delete`) in the `ToolRegistry`.
   - Construct `SchedulerExecutor` with the store, turn runner, config, and cancellation token.
   - Attach executor to gateway server via `with_scheduler()`.

This follows the same pattern used for memory tools registration.

---

### Step 12: System Prompt Augmentation

When scheduler tools are registered, add a brief instruction to the system prompt:

```
You can create and manage scheduled tasks that run automatically.
- Use schedule_create to set up one-off or recurring tasks.
- Use schedule_search to find existing schedules (supports filtering and pagination).
- Use schedule_edit to modify, pause, or resume schedules.
- Use schedule_delete to permanently remove a schedule.

Each scheduled task executes as an independent agent turn with its own context.
Write goals as complete, self-contained instructions — scheduled tasks run
without conversational history from this session.
```

This is injected alongside the existing system prompt, not replacing it.

---

### Step 13: Testing Strategy

#### Unit Tests (per crate):

1. **`crates/types`**: Serde round-trip for `ScheduleCadence`, `NotificationPolicy`, `ScheduleDefinition`, `ScheduleStatus`, `SchedulePatch`. Validation tests for config defaults.

2. **`crates/memory`**:
   - Migration applies cleanly (extends existing migration test).
   - `SchedulerStore` CRUD: create, get, search, delete, count.
   - `search_schedules` with various filter combinations: name, status, cadence_type, notification.
   - `search_schedules` pagination: correct `total_count`, `offset`, `limit` behavior.
   - `update_schedule` partial patch: only specified fields change, others untouched.
   - `update_schedule` pause/resume: `next_run_at` cleared on pause, recomputed on resume.
   - `update_schedule` cadence change: `next_run_at` recomputed.
   - `due_schedules` correctly filters by `next_run_at` and `status = active`.
   - `record_run_and_reschedule` atomically updates and prunes.
   - Run history pruning respects retention limit.
   - User schedule count limit enforcement.
   - Cascading delete removes run history.

3. **`crates/memory` cadence module**:
   - `next_run_for_cadence` with Cron, Once, Interval variants.
   - `next_run_for_cadence` returns None for past one-shot.
   - `validate_cadence` rejects past one-off times, too-frequent intervals/cron.
   - Timezone handling: `Asia/Kolkata` default, various IANA zones.
   - Invalid timezone returns error.

4. **`crates/tools`**:
   - `schedule_create`: validates inputs, rejects invalid cron, respects limits, uses default timezone `Asia/Kolkata`.
   - `schedule_create`: does NOT accept max_turns or max_cost params.
   - `schedule_search`: no filters returns all, each filter works individually and combined, pagination math correct.
   - `schedule_search`: `remaining` and `hint` fields are accurate.
   - `schedule_edit`: partial updates, pause/resume toggles, cadence change recomputes next_run.
   - `schedule_edit`: ownership check rejects unauthorized edits.
   - `schedule_edit`: cannot resume a completed one-shot without new cadence.
   - `schedule_delete`: ownership check, cascading cleanup.
   - All tools require `user_id` in `ToolExecutionContext`.

#### Integration Tests:

5. **`crates/runtime`**:
   - `SchedulerExecutor::tick()` with MockProvider + MockSchedulerStore: due schedule triggers a turn, response captured, run recorded.
   - Conditional notification: response with `[NOTIFY]` triggers notification, without does not.
   - Budget enforcement: scheduled turn uses `SchedulerConfig.max_turns` (10 default), not any LLM-provided value.
   - One-shot auto-complete after execution (status → Completed, next_run_at → None).
   - Consecutive failure tracking: auto-disable after threshold reached.
   - Concurrent execution respects `max_concurrent`.
   - Paused schedules are not picked up by `due_schedules`.

6. **End-to-end** (gateway-level):
   - LLM creates a schedule via tool → schedule appears in store → executor fires it → run recorded.
   - Notification routing through gateway broadcast.
   - Schedule search with pagination across multiple schedules.
   - Edit → pause → verify not firing → resume → verify firing again.

---

### Step 14: Dependency Additions

**`Cargo.toml` changes:**

- `crates/memory/Cargo.toml`: Add `cron = "0.15"` (for cron expression parsing), `chrono-tz = "0.10"` (for timezone support).
- No new dependencies in other crates — they use types re-exported through `types`.

---

## Implementation Order

| Order | Step | Crate(s) | Depends On |
|-------|------|----------|------------|
| 1 | Types (scheduler.rs) | `types` | — |
| 2 | Error variants | `types` | Step 1 |
| 3 | Config (SchedulerConfig) | `types` | Step 1 |
| 4 | DB migrations | `memory` | — |
| 5 | SchedulerStore trait + impl | `memory` | Steps 1, 2, 4 |
| 6 | Cadence evaluation | `memory` or `types` | Step 1 |
| 7 | Scheduler tools (4 tools) | `tools` | Steps 1, 2, 3, 5, 6 |
| 8 | SchedulerExecutor | `runtime` | Steps 5, 6 |
| 9 | Notification routing | `runtime`, `types` | Step 8 |
| 10 | Gateway integration | `gateway` | Steps 8, 9 |
| 11 | Runner bootstrap | `runner` | Steps 7, 10 |
| 12 | System prompt | `runtime` | Step 7 |
| 13 | Tests | all | Steps 1-12 |
| 14 | Dependencies | — | Step 6 |

Steps 1-3 can be done in parallel. Steps 4-6 can be done in parallel. Step 7 depends on the store. Step 8 depends on the store + cadence. Steps 9-12 are sequential.

---

## Key Design Decisions & Rationale

| Decision | Rationale |
|----------|-----------|
| **Schedules fire as agent turns (prompt-based), not shell commands** | Reuses entire runtime policy envelope; the LLM decides *how* to accomplish the goal using available tools |
| **Each schedule gets its own `runtime_session_id`** | Prevents history pollution between scheduled runs and interactive sessions; allows periodic tasks to accumulate context |
| **Cadence type is an enum, not freeform** | Prevents LLM hallucination of hybrid formats; our code validates and computes |
| **`max_turns`/`max_cost` are operator-only config, not in tool schema** | Prevents runaway scheduled executions; the LLM should not control its own budget for unsupervised runs |
| **Default timezone is `Asia/Kolkata`** | Primary user timezone; always explicit in the stored cadence (no ambient timezone ambiguity) |
| **`schedule_search` with pagination instead of simple list** | Scales to many schedules; `remaining`/`hint` guide the LLM's pagination behavior naturally |
| **`schedule_edit` for pause/resume and mutations** | Single tool with partial-patch semantics is cleaner than separate pause/resume/update tools |
| **`[NOTIFY]` marker for conditional notification** | Simple, robust, no structured output needed; easy for LLMs to follow |
| **Polling executor (not event-driven)** | Simple, reliable, adequate for expected cadence (15s granularity); avoids timer-wheel complexity |
| **Separate migrations (not schema wipe)** | Follows existing precedent; additive migrations preserve existing data |
| **Conservative defaults (disabled, 10 turns, min 60s interval)** | Scheduled runs consume tokens/money without human oversight; safety first |
| **Auto-disable after consecutive failures** | Prevents unbounded retries of broken schedules; operator/user must investigate and re-enable |
| **Store trait for testability** | `MockSchedulerStore` enables executor unit tests without DB |

---

## Edge Cases & Special Considerations

### 1. Clock Skew / Missed Ticks
If the server restarts, schedules that were due during downtime are picked up on the next poll (their `next_run_at` is in the past). They execute once, not retroactively for every missed tick. The `MissedTickBehavior::Skip` on the tokio interval prevents pile-up.

### 2. Concurrent Edit + Execution Race
If a user edits a schedule while it's executing, the execution uses the snapshot taken at tick time. The edit's `next_run_at` recomputation happens after the edit, and `record_run_and_reschedule` also recomputes. To avoid conflicts, `record_run_and_reschedule` uses a conditional update: `UPDATE ... WHERE last_run_at < ?` so it doesn't overwrite a more recent edit's `next_run_at`.

### 3. One-Shot Completion
When a `Once` schedule fires successfully, it's marked `Completed` with `next_run_at = None`. It remains visible in `schedule_search` (for audit) but never fires again. Users can delete it if they want.

### 4. LLM Creating Schedules That Create Schedules
A scheduled turn has access to the same tools including `schedule_create`. This is by design (a schedule could create sub-schedules). The per-user limit (`max_schedules_per_user`) prevents unbounded growth. If this is a concern, scheduler tools can be excluded from the scheduled turn's tool registry as a future enhancement.

### 5. Goal Prompt Injection
The `goal` is user-authored content that becomes the user message in a scheduled turn. Standard prompt injection defenses apply (the system prompt boundary is maintained). The scheduled turn's system prompt explicitly states it's a scheduled execution, which helps the LLM maintain context.

### 6. Long-Running Scheduled Turns
The `max_turns` limit (default 10) caps execution length. The `max_cost` caps spend. The child `CancellationToken` allows the executor to abort a stuck turn. Additionally, each turn within the runtime already has its own timeout via the tool execution layer.

---

## Verification Gate

Phase 19 is complete when:
1. ✅ One-off schedule created via tool → fires at specified time → bounded turn executes → run recorded
2. ✅ Periodic (cron) schedule fires repeatedly with correct next_run computation
3. ✅ Conditional notification: `[NOTIFY]` response routes to user, non-notify response is silent
4. ✅ Budget enforcement: scheduled turns use operator-configured `max_turns` (10) / `max_cost`, not LLM-overridable
5. ✅ Schedule isolation: scheduled runs don't affect interactive session context
6. ✅ `schedule_search` returns paginated results with correct `remaining` / `hint`
7. ✅ `schedule_edit` can pause, resume, change cadence, change goal
8. ✅ Auto-disable after consecutive failures works correctly
9. ✅ All existing tests pass (no regressions)
10. ✅ `cargo clippy`, `cargo fmt`, `cargo test` all green
