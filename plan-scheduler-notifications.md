# Plan: Origin-Only Scheduler Notifications + Full Run Output (History)

## Context

When a scheduled task fires, the output often never reaches the user who created it. Root causes today:

1. `ScheduleDefinition` only stores `user_id` — no stable delivery target.
2. `GatewayServer::notify_user` broadcasts only to **in-memory** WebSocket sessions — Telegram users often have no active WS subscriber, so they never receive anything.
3. Run output is truncated to 500 chars; full output is discarded and unqueryable.
4. `SchedulerNotifier` is synchronous and uses `try_read()` locks, which can silently drop notifications under contention.

## Target UX / Product Semantics

**Origin-only routing** (explicitly chosen):

- Scheduled notifications are delivered **only** to the channel/context where the schedule was created.
  - Telegram: the originating chat/topic (`chat_id` + optional `message_thread_id`).
  - TUI: the originating gateway `session_id`.
- Legacy schedules created before this change (no origin fields) keep the current behavior (broadcast to all in-memory WS sessions).
- Additionally, operational failure notifications can be emitted when a schedule fails repeatedly (see “Notify on N failures”).

This avoids duplicate notifications and makes delivery predictable (“it goes where I created it”).

## Approach Summary

1. Persist the schedule’s origin (`channel_id`, `channel_context_id`) at creation time.
2. Propagate origin **per turn** from the ingress path into `ToolExecutionContext` (no reverse lookup via `session_id`).
3. Store full run output (`schedule_runs.output`) and provide tools to retrieve it safely (without tool-output truncation issues).
4. Make `SchedulerNotifier` **async** to avoid `try_read()` drops.
5. Route notifications origin-only:
   - `tui` → publish to the originating in-memory session (if connected).
   - non-`tui` → delegate to a registered `ProactiveSender` (Telegram).
6. Improve Telegram proactive send UX: robust context parsing, message splitting, and HTML parse mode.
7. Strip `[NOTIFY]` marker from stored output and user-visible notifications.
8. Notify on N consecutive failures (configurable threshold).

---

## Step 1: Database Migrations

### 1a) Add origin fields to schedules

**New file: `crates/memory/migrations/0022_add_channel_origin_to_schedules.sql`**
```sql
ALTER TABLE schedules ADD COLUMN channel_id TEXT;
ALTER TABLE schedules ADD COLUMN channel_context_id TEXT;
```

### 1b) Add full output to schedule run records

**New file: `crates/memory/migrations/0023_add_full_output_to_schedule_runs.sql`**
```sql
ALTER TABLE schedule_runs ADD COLUMN output TEXT;
```

Register both migrations in `crates/memory/src/schema.rs` (`MIGRATIONS` array, after entry 0021).

---

## Step 2: Type + Config Changes

### 2a) Persisted schedule definition

**`crates/types/src/scheduler.rs`** — add to `ScheduleDefinition`:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub channel_id: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub channel_context_id: Option<String>,
```

### 2b) Run record output

**`crates/types/src/scheduler.rs`** — add to `ScheduleRunRecord`:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub output: Option<String>,
```

### 2c) Per-turn tool execution context

**`crates/types/src/tool.rs`** — add to `ToolExecutionContext`:
```rust
/// Channel that originated the current turn (e.g. "telegram", "tui").
pub channel_id: Option<String>,
/// Channel-context identifier within that channel.
/// - Telegram: "{chat_id}" or "{chat_id}:{thread_id}"
/// - TUI: gateway `session_id`
pub channel_context_id: Option<String>,
```

### 2d) SchedulerConfig: notify on N consecutive failures

**`crates/types/src/config.rs`** — add field to `SchedulerConfig`:
```rust
/// Notify the user after N consecutive failures for a schedule.
/// 0 disables failure notifications. Default: 3.
#[serde(default = "default_scheduler_notify_after_failures")]
pub notify_after_failures: u32,
```

Add a default function and wire into `impl Default for SchedulerConfig`.

---

## Step 3: Per-Turn Origin Propagation (No Reverse Lookup)

### Why

Reverse lookup (`session_id -> channel_context_id`) is ambiguous because multiple channel contexts can map to the same gateway session (e.g. Telegram `/switch` can point multiple chats at one session). Origin must be captured from the **actual ingress** that submitted the turn.

### 3a) Gateway: carry per-turn origin into the turn runner

**`crates/gateway/src/lib.rs`**

- Add an internal helper struct (gateway-private):
  ```rust
  struct TurnOrigin {
      channel_id: String,
      channel_context_id: Option<String>,
  }
  ```

- Thread `TurnOrigin` through the turn start pipeline:
  - WebSocket handler sets:
    - `channel_id = "tui"`
    - `channel_context_id = Some(session.session_id.clone())`
  - In-process adapters can pass their own:
    - Telegram sets `channel_id = "telegram"` and `channel_context_id = Some(derive_channel_context_id(chat_id, thread_id))`

- Add a new internal API for in-process adapters:
  ```rust
  pub async fn submit_turn_from_channel(
      &self,
      session: &Arc<SessionState>,
      send_turn: GatewaySendTurn,
      origin_channel_id: &str,
      origin_channel_context_id: Option<&str>,
  ) -> Option<GatewayServerFrame>;
  ```

This keeps the external WS protocol unchanged and avoids WS clients spoofing cross-channel origin routing.

### 3b) Channel capabilities should reflect the ingress channel

In `start_turn`, compute `channel_capabilities` from `origin.channel_id` (not `session.channel_origin`) so that `/switch` cross-channel scenarios still get the correct capabilities.

### 3c) Turn runner: populate `ToolExecutionContext` from the per-turn origin

**`crates/gateway/src/turn_runner.rs`**

- Extend `GatewayTurnRunner::run_turn` signature to accept per-turn origin fields:
  ```rust
  async fn run_turn(
      &self,
      user_id: &str,
      session_id: &str,
      input: UserTurnInput,
      cancellation: CancellationToken,
      delta_sender: mpsc::UnboundedSender<StreamItem>,
      channel_capabilities: Option<ChannelCapabilities>,
      origin_channel_id: Option<String>,
      origin_channel_context_id: Option<String>,
  ) -> Result<Response, RuntimeError>;
  ```

- In `RuntimeGatewayTurnRunner::run_turn`, set:
  - `tool_context.channel_id = origin_channel_id`
  - `tool_context.channel_context_id = origin_channel_context_id`

Scheduled turns (`run_scheduled_turn`) pass `None` for both.

---

## Step 4: Capture Origin at Schedule Creation

**`crates/tools/src/scheduler_tools.rs`** — in `ScheduleCreateTool::execute`, when building `ScheduleDefinition`:

```rust
let def = ScheduleDefinition {
    // ... existing fields ...
    channel_id: context.channel_id.clone(),
    channel_context_id: context.channel_context_id.clone(),
};
```

Optionally include these in tool JSON output (useful for debugging) but keep them opaque to users.

---

## Step 5: Scheduler Store Updates (LibsqlSchedulerStore)

**`crates/memory/src/scheduler_store.rs`**

### 5a) `create_schedule` INSERT

Add `channel_id` and `channel_context_id` columns + binds.

### 5b) All SELECTs for schedules

Update `schedule_from_row` and SELECT column lists in:
- `get_schedule`
- `due_schedules`
- `search_schedules`

### 5c) `record_run_and_reschedule`

Add `output` column to `schedule_runs` insert and bind the full (cleaned) response.

### 5d) `get_run_history`

Include `output` in SELECT and parse via `run_record_from_row`.

---

## Step 6: Scheduler Executor Changes (Async Notifier + Clean Output + Failure Notifications)

**`crates/runtime/src/scheduler_executor.rs`**

### 6a) Make `SchedulerNotifier` async

Change trait to:
```rust
use async_trait::async_trait;

#[async_trait]
pub trait SchedulerNotifier: Send + Sync {
    async fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame);
}
```

Update call sites to `await`.

### 6b) Strip `[NOTIFY]` marker early

- Compute a `clean_text`:
  - If `Conditional` and response starts with `[NOTIFY]`, strip it and leading whitespace.
  - Otherwise, `clean_text = response_text`.

Use `clean_text` consistently for:
- notification `message`
- `output_summary`
- persisted `output`

### 6c) Persist full output

Add `output: Option<String>` to `ScheduleRunRecord`:
```rust
let output = if clean_text.is_empty() { None } else { Some(clean_text.clone()) };
```

Keep `output_summary` truncated to 500 chars for fast display.

### 6d) Notify on N consecutive failures

- On failure, compute `new_consecutive_failures = schedule.consecutive_failures + 1`.
- If `config.notify_after_failures > 0` and `new_consecutive_failures == config.notify_after_failures`, send a `ScheduledNotification` (origin-only) with a message like:
  - `"❌ Scheduled task '<name>' has failed N times in a row. Latest error: <summary>"`
- If the schedule is auto-disabled (`new_consecutive_failures >= auto_disable_after_failures`), also send a notification:
  - `"⛔ Scheduled task '<name>' was disabled after N consecutive failures."`

These are operational notifications and should be sent regardless of `NotificationPolicy`.

---

## Step 7: ProactiveSender Trait (Types) + Origin-Only Gateway Routing

### 7a) ProactiveSender trait

**`crates/types/src/proactive.rs`** (new) and re-export from `crates/types/src/lib.rs`:
```rust
use crate::GatewayServerFrame;

pub trait ProactiveSender: Send + Sync {
    /// Send a server frame to the specified channel context.
    fn send_proactive(&self, channel_context_id: &str, frame: &GatewayServerFrame);
}
```

### 7b) Gateway proactive sender registry

**`crates/gateway/src/lib.rs`**

- Add registry:
  ```rust
  proactive_senders: RwLock<HashMap<String, Arc<dyn ProactiveSender>>>,
  ```
- Add registration:
  ```rust
  pub fn register_proactive_sender(
      &self,
      channel_id: impl Into<String>,
      sender: Arc<dyn ProactiveSender>,
  );
  ```

### 7c) Origin-only routing implementation (async)

Update `impl SchedulerNotifier for GatewayServer`:

- If `schedule.channel_id` is set:
  - If `channel_id == "tui"`:
    - Interpret `channel_context_id` as `session_id` and publish only to that in-memory session (if present).
  - Else:
    - Look up `proactive_senders[channel_id]` and call `send_proactive(channel_context_id, &frame)`.
  - **Do not** broadcast as fallback (origin-only).
  - If delivery is impossible (missing sender / missing ctx), log a warning.

- If schedule has no origin fields (legacy):
  - fallback to current behavior: broadcast to all in-memory sessions for `schedule.user_id`.

Because notifier is async, use `read().await` locks (no `try_read()` drops).

---

## Step 8: Telegram Proactive Sender (Length Splitting + Parse Mode + Robust Context Parsing)

**`crates/channels/src/telegram.rs`**

Add a lightweight proactive sender:

- `TelegramProactiveSender::new(bot_token, max_message_length)`
- `send_proactive()` should:
  1. Parse `channel_context_id` safely:
     - Accept `"{chat_id}"` or `"{chat_id}:{thread_id}"`
     - If parse fails, log warn and return (do NOT default to 0)
  2. Render message:
     - Use `ParseMode::Html`
     - Reuse existing formatting helpers (recommend: expose `split_message()` + `markdown_to_telegram_html()` as `pub(crate)` so adapter + proactive sender share behavior)
  3. Split long notifications:
     - Respect `max_message_length`
     - Send multiple messages if needed (chunked)

**`crates/runner/src/bin/oxydra-vm.rs`**

When Telegram is enabled, register the proactive sender before spawning the adapter:
```rust
let proactive_sender = Arc::new(TelegramProactiveSender::new(&bot_token, telegram_config.max_message_length));
gateway.register_proactive_sender("telegram", proactive_sender);
```

---

## Step 9: Run History Tools (Safe Under Tool Output Truncation)

Tool output is truncated (~16KB) by the tool registry. Returning “full output for N runs” can easily exceed this and produce truncated/invalid JSON. The history UX should be chunk-friendly.

### 9a) `schedule_runs` tool (list runs)

**New tool:** `schedule_runs`

- Required: `schedule_id`
- Optional: `limit` (1..=20, default 5)

Return metadata per run (always safe):
- `run_id`, timestamps, status, `output_summary`, `notified`
- do **not** include full output by default

### 9b) `schedule_run_output` tool (chunked output)

**New tool:** `schedule_run_output`

Schema:
```json
{
  "type": "object",
  "required": ["run_id"],
  "properties": {
    "run_id": {"type": "string", "minLength": 1},
    "offset": {"type": "integer", "minimum": 0, "default": 0},
    "max_chars": {"type": "integer", "minimum": 200, "maximum": 8000, "default": 4000}
  }
}
```

Execution:
1. Resolve `user_id` from context
2. Look up the run by `run_id` (add store method below) and verify the owning schedule’s `user_id`
3. Return `{ run_id, offset, max_chars, chunk, remaining }`

### 9c) Store additions

**`crates/memory/src/scheduler_store.rs`** (trait + libsql impl):

Add:
```rust
async fn get_run_by_id(&self, run_id: &str) -> Result<ScheduleRunRecord, SchedulerError>;
```

(Alternatively: `get_run_output_chunk(run_id, offset, max_chars)` to avoid fetching full output; either is fine.)

---

## Step 10: Documentation Updates (Origin-Only + New Tools)

Update guidebook to reflect the new UX semantics:

- **`docs/guidebook/09-gateway-and-channels.md`**
  - Scheduler Notification section: origin-only routing, proactive sender concept, TUI session-scoped delivery.

- **`docs/guidebook/05-agent-runtime.md`**
  - Scheduled Turn Execution: async notifier, stripping `[NOTIFY]`, failure notifications (`notify_after_failures`).

- **`docs/guidebook/04-tool-system.md`**
  - Scheduler tools: add `schedule_runs` + `schedule_run_output`, and note tool-output truncation constraints.

---

## Files to Modify (Updated)

| File | Change |
|---|---|
| `crates/memory/migrations/0022_*.sql` | New — add `channel_id`, `channel_context_id` to schedules |
| `crates/memory/migrations/0023_*.sql` | New — add `output` to schedule_runs |
| `crates/memory/src/schema.rs` | Register migrations 0022, 0023 |
| `crates/types/src/scheduler.rs` | Add origin fields to `ScheduleDefinition`; add `output` to `ScheduleRunRecord` |
| `crates/types/src/tool.rs` | Add `channel_id`, `channel_context_id` to `ToolExecutionContext` |
| `crates/types/src/config.rs` | Add `notify_after_failures` to `SchedulerConfig` |
| `crates/tools/src/scheduler_tools.rs` | Capture origin in `schedule_create`; add `schedule_runs` + `schedule_run_output` tools |
| `crates/memory/src/scheduler_store.rs` | SQL updates for new columns + add `get_run_by_id` (or chunk getter) |
| `crates/runtime/src/scheduler_executor.rs` | Make notifier async; strip `[NOTIFY]`; store full output; notify-on-failures |
| `crates/runtime/src/lib.rs` | Re-export updated `SchedulerNotifier` |
| `crates/types/src/proactive.rs` | New — `ProactiveSender` trait |
| `crates/types/src/lib.rs` | Re-export `ProactiveSender` |
| `crates/gateway/src/lib.rs` | Proactive sender registry; origin-only async routing; per-turn origin plumbing + new submit API |
| `crates/gateway/src/turn_runner.rs` | Accept origin params; populate `ToolExecutionContext` |
| `crates/channels/src/telegram.rs` | Add `TelegramProactiveSender` with splitting + HTML parse mode |
| `crates/runner/src/bin/oxydra-vm.rs` | Wire proactive sender registration |
| `docs/guidebook/*.md` | Update documentation for origin-only + new tools |

---

## Backward Compatibility

- Existing schedules with `channel_id = NULL` continue to use legacy broadcast-to-in-memory behavior.
- Existing `schedule_runs` rows with `output = NULL` return `null` output; `schedule_run_output` should return a clear “no output stored for this run” message.
- New config field uses `#[serde(default)]` and won’t break existing config files.

---

## Verification

1. **Build**: `cargo build`
2. **Clippy**: `cargo clippy --all-targets --all-features` (or at least touched crates)
3. **Tests**: `cargo test` (update mocks for async notifier)
4. **Manual Telegram test**:
   - Create schedule from Telegram chat/topic
   - Wait for it to fire
   - Verify notification arrives in the same chat/topic
5. **Manual TUI test**:
   - Create schedule from a specific TUI session
   - Wait for it to fire
   - Verify only that session (if connected) receives it
6. **Failure notify test**:
   - Create schedule that fails (e.g., invalid tool usage / forced error)
   - Ensure a notification is sent exactly when consecutive failures hits `notify_after_failures`
7. **History tool test**:
   - List runs with `schedule_runs`
   - Fetch full output via `schedule_run_output` in chunks; verify no JSON truncation issues
