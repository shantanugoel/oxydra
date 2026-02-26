# Plan: Route Scheduler Output to Originating Channel + Run History

## Context

When a scheduled task fires, the output never reaches the user who created it. Root causes:
1. `ScheduleDefinition` only stores `user_id` — no channel/session origin info
2. `GatewayServer::notify_user` broadcasts to all **in-memory** WebSocket sessions — Telegram users never receive anything because Telegram has no persistent subscription
3. Run output is truncated to 500 chars; full output is discarded and unqueryable

## Approach

Store the originating channel info (`channel_id`, `channel_context_id`) on each schedule at creation time. At notification time, use this to route proactively to the correct channel (Telegram chat/thread or TUI session). Also store full run output and expose a `schedule_runs` tool for retrieval.

---

## Step 1: Database Migrations

**New file: `crates/memory/migrations/0022_add_channel_origin_to_schedules.sql`**
```sql
ALTER TABLE schedules ADD COLUMN channel_id TEXT;
ALTER TABLE schedules ADD COLUMN channel_context_id TEXT;
```

**New file: `crates/memory/migrations/0023_add_full_output_to_schedule_runs.sql`**
```sql
ALTER TABLE schedule_runs ADD COLUMN output TEXT;
```

Register both in `crates/memory/src/schema.rs` (the `MIGRATIONS` array, after entry 0021).

---

## Step 2: Type Changes

**`crates/types/src/scheduler.rs`** — Add to `ScheduleDefinition`:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub channel_id: Option<String>,
#[serde(default, skip_serializing_if = "Option::is_none")]
pub channel_context_id: Option<String>,
```

Add to `ScheduleRunRecord`:
```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub output: Option<String>,
```

**`crates/types/src/tool.rs`** — Add to `ToolExecutionContext`:
```rust
/// Channel that originated the current session (e.g. "telegram", "tui").
pub channel_id: Option<String>,
/// Channel-context identifier within that channel (e.g. "12345:42" for a Telegram thread).
pub channel_context_id: Option<String>,
```

---

## Step 3: Reverse Lookup — Session → Channel

The existing `SessionStore` trait has `get_channel_session(channel_id, context_id) -> session_id` but we need the reverse: given a `session_id`, find the `(channel_id, channel_context_id)`.

**`crates/types/src/session.rs`** — Add to `SessionStore` trait:
```rust
/// Reverse lookup: find which channel/context owns a given session.
async fn get_session_channel(
    &self,
    session_id: &str,
) -> Result<Option<(String, String)>, MemoryError>;
```

**`crates/memory/src/session_store.rs`** — Implement in `LibsqlSessionStore`:
```sql
SELECT channel_id, channel_context_id
FROM channel_session_mappings
WHERE session_id = ?1
LIMIT 1
```

---

## Step 4: Populate Channel Info in ToolExecutionContext

Currently, `RuntimeGatewayTurnRunner` builds a `ToolExecutionContext` with only `user_id` and `session_id`. We need to also populate `channel_id` and `channel_context_id` so tools (specifically `schedule_create`) can capture which channel the user is on.

**`crates/gateway/src/turn_runner.rs`** — `RuntimeGatewayTurnRunner`:
- Add field: `session_store: Option<Arc<dyn SessionStore>>`
- In `new()`, accept the session store as an additional parameter
- In `run_turn()`, after building `tool_context` with `user_id`/`session_id`, look up channel info:
  ```rust
  if let Some(ref store) = self.session_store {
      if let Ok(Some((ch_id, ctx_id))) = store.get_session_channel(session_id).await {
          tool_context.channel_id = Some(ch_id);
          tool_context.channel_context_id = Some(ctx_id);
      }
  }
  ```

**`crates/runner/src/bin/oxydra-vm.rs`** — Pass `session_store` clone when constructing `RuntimeGatewayTurnRunner`:
```rust
RuntimeGatewayTurnRunner::new(runtime, provider, model, session_store_for_turn_runner)
```

---

## Step 5: Capture Channel at Schedule Creation

**`crates/tools/src/scheduler_tools.rs`** — In `ScheduleCreateTool::execute`:
When building the `ScheduleDefinition`, read from the tool execution context:
```rust
let def = ScheduleDefinition {
    // ... existing fields ...
    channel_id: context.channel_id.clone(),
    channel_context_id: context.channel_context_id.clone(),
};
```

This is purely additive — no existing logic changes.

---

## Step 6: Store Changes (LibsqlSchedulerStore)

**`crates/memory/src/scheduler_store.rs`**:

### 6a. `create_schedule` INSERT
Add `channel_id` and `channel_context_id` columns and bind parameters to the INSERT statement.

### 6b. All SELECT queries
Update `schedule_from_row` to read the two new columns. Update SELECT column lists in:
- `get_schedule`
- `due_schedules`
- `search_schedules`

### 6c. `record_run_and_reschedule`
Add `output` column to the INSERT into `schedule_runs`. Bind the full response text.

### 6d. `get_run_history` SELECT
Add `output` to the column list. Update `run_record_from_row` to read it.

---

## Step 7: ProactiveSender Trait

Introduce a trait that allows the gateway to delegate proactive message delivery to channel adapters without depending on the `channels` crate directly.

**`crates/types/src/lib.rs`** (or new file `crates/types/src/proactive.rs`):
```rust
use crate::GatewayServerFrame;

/// Trait for channel adapters that can proactively push notifications
/// (e.g. scheduled task results) without an active user request.
pub trait ProactiveSender: Send + Sync {
    /// Send a server frame to the specified channel context.
    /// `channel_context_id` uses the same format as `channel_session_mappings`.
    fn send_proactive(&self, channel_context_id: &str, frame: &GatewayServerFrame);
}
```

---

## Step 8: SchedulerNotifier Signature Change

**`crates/runtime/src/scheduler_executor.rs`** — Change `SchedulerNotifier` trait:
```rust
pub trait SchedulerNotifier: Send + Sync {
    fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame);
}
```

Update `handle_notification` call site to pass `schedule` instead of `&schedule.user_id`.

Also in `execute_schedule`: store full output in the run record:
```rust
let output = if response_text.is_empty() { None } else { Some(response_text.clone()) };
// output_summary remains the truncated version for quick display
let run_record = ScheduleRunRecord {
    // ... existing fields ...
    output_summary,
    output,
};
```

---

## Step 9: Gateway Notification Routing

**`crates/gateway/src/lib.rs`**:

### 9a. Add proactive sender registry to `GatewayServer`:
```rust
pub struct GatewayServer {
    // ... existing fields ...
    proactive_senders: RwLock<HashMap<String, Arc<dyn ProactiveSender>>>,
}
```

### 9b. Add registration method:
```rust
pub fn register_proactive_sender(
    &self,
    channel_id: impl Into<String>,
    sender: Arc<dyn ProactiveSender>,
) { ... }
```

### 9c. Update `SchedulerNotifier` implementation:
```rust
impl SchedulerNotifier for GatewayServer {
    fn notify_user(&self, schedule: &ScheduleDefinition, frame: GatewayServerFrame) {
        // If schedule has specific channel info, try targeted delivery
        if let (Some(ch_id), Some(ctx_id)) = (&schedule.channel_id, &schedule.channel_context_id) {
            // For non-TUI channels, delegate to proactive sender
            if ch_id != GATEWAY_CHANNEL_ID {
                if let Ok(senders) = self.proactive_senders.try_read() {
                    if let Some(sender) = senders.get(ch_id.as_str()) {
                        sender.send_proactive(ctx_id, &frame);
                        return;
                    }
                }
            }
        }

        // Fallback: broadcast to all in-memory WS sessions (current behavior)
        if let Ok(users) = self.users.try_read()
            && let Some(user) = users.get(&schedule.user_id)
            && let Ok(sessions) = user.sessions.try_read()
        {
            for session in sessions.values() {
                session.publish(frame.clone());
            }
        }
    }
}
```

---

## Step 10: Telegram Proactive Sender

**`crates/channels/src/telegram.rs`** — New lightweight struct (separate from `TelegramAdapter` which is consumed by `run()`):

```rust
pub struct TelegramProactiveSender {
    bot: Bot,
}

impl TelegramProactiveSender {
    pub fn new(bot_token: &str) -> Self {
        Self { bot: Bot::new(bot_token) }
    }
}

impl ProactiveSender for TelegramProactiveSender {
    fn send_proactive(&self, channel_context_id: &str, frame: &GatewayServerFrame) {
        let GatewayServerFrame::ScheduledNotification(ref notif) = frame else { return };

        let (chat_id, thread_id) = parse_channel_context_id(channel_context_id);
        let message = format!(
            "📋 Scheduled task '{}' completed:\n\n{}",
            notif.schedule_name.as_deref().unwrap_or(&notif.schedule_id),
            notif.message
        );

        let bot = self.bot.clone();
        tokio::spawn(async move {
            let params = SendMessageParams {
                chat_id: ChatId::Integer(chat_id),
                text: message,
                message_thread_id: thread_id,
                ..Default::default()
            };
            if let Err(e) = bot.send_message(&params).await {
                tracing::warn!(error = %e, "failed to send proactive scheduled notification");
            }
        });
    }
}
```

Also add a helper to parse `channel_context_id` back to `(chat_id, thread_id)`:
```rust
fn parse_channel_context_id(ctx: &str) -> (i64, Option<i32>) {
    if let Some((chat, thread)) = ctx.split_once(':') {
        (chat.parse().unwrap_or(0), thread.parse().ok())
    } else {
        (ctx.parse().unwrap_or(0), None)
    }
}
```

**`crates/runner/src/bin/oxydra-vm.rs`** — In `spawn_telegram_adapter`, before spawning the adapter:
```rust
let proactive_sender = Arc::new(
    channels::telegram::TelegramProactiveSender::new(&bot_token)
);
gateway.register_proactive_sender("telegram", proactive_sender);
```

---

## Step 11: `schedule_runs` Tool (History Retrieval)

**`crates/tools/src/scheduler_tools.rs`** — New tool:

```rust
pub const SCHEDULE_RUNS_TOOL_NAME: &str = "schedule_runs";

pub struct ScheduleRunsTool {
    store: Arc<dyn SchedulerStore>,
}
```

**Schema:**
```json
{
    "type": "object",
    "required": ["schedule_id"],
    "properties": {
        "schedule_id": { "type": "string", "minLength": 1 },
        "limit": { "type": "integer", "minimum": 1, "maximum": 20, "default": 5 }
    }
}
```

**Execute:**
1. Parse args, resolve `user_id` from context
2. Fetch schedule via `store.get_schedule(schedule_id)` — verify ownership
3. Call `store.get_run_history(schedule_id, limit)`
4. Return JSON with run records including full `output`

**Registration:** Add alongside existing scheduler tools in `register_scheduler_tools()`.

---

## Files to Modify

| File | Change |
|---|---|
| `crates/memory/migrations/0022_*.sql` | **New** — ALTER schedules table |
| `crates/memory/migrations/0023_*.sql` | **New** — ALTER schedule_runs table |
| `crates/memory/src/schema.rs` | Register migrations 0022, 0023 |
| `crates/types/src/scheduler.rs` | Add `channel_id`, `channel_context_id` to `ScheduleDefinition`; add `output` to `ScheduleRunRecord` |
| `crates/types/src/tool.rs` | Add `channel_id`, `channel_context_id` to `ToolExecutionContext` |
| `crates/types/src/session.rs` | Add `get_session_channel` to `SessionStore` trait |
| `crates/types/src/lib.rs` | Export `ProactiveSender` trait |
| `crates/memory/src/session_store.rs` | Implement `get_session_channel` |
| `crates/memory/src/scheduler_store.rs` | All SQL changes for new columns in both tables |
| `crates/tools/src/scheduler_tools.rs` | Channel capture in `schedule_create` + new `schedule_runs` tool |
| `crates/runtime/src/scheduler_executor.rs` | `SchedulerNotifier` signature change, full output storage |
| `crates/runtime/src/lib.rs` | Re-export updated `SchedulerNotifier` |
| `crates/gateway/src/lib.rs` | Proactive sender registry + routing in `notify_user` |
| `crates/gateway/src/turn_runner.rs` | Inject session store, populate channel info in tool context |
| `crates/channels/src/telegram.rs` | New `TelegramProactiveSender` + `parse_channel_context_id` helper |
| `crates/runner/src/bin/oxydra-vm.rs` | Wire proactive sender registration + session store to turn runner |

---

## Backward Compatibility

- Old schedules with `channel_id = NULL` → falls back to broadcast-to-all-sessions (current behavior)
- Old `schedule_runs` rows with `output = NULL` → `schedule_runs` tool returns null for those
- `ToolExecutionContext` new fields default to `None` via `#[derive(Default)]` — no breakage for non-channel callers

---

## Design Decisions

**Why `channel_id` + `channel_context_id` rather than `session_id`?**
Session IDs can change (e.g. Telegram `/new` command creates a new session for the same chat). The `(channel_id, channel_context_id)` pair is stable and maps directly to the physical chat/thread identity. For Telegram, `channel_context_id` is directly parseable to `(chat_id, thread_id)`.

**Why a separate `TelegramProactiveSender` struct?**
`TelegramAdapter` is consumed by `run()` which wraps it in `Arc<Self>` internally. A separate lightweight struct sharing the same `Bot` (which is `Clone` in frankenstein) avoids `Arc<Mutex>` gymnastics.

**Why keep `SchedulerNotifier::notify_user` synchronous?**
The Telegram send can be fire-and-forget via `tokio::spawn` inside `send_proactive`. Keeping the trait sync avoids making the scheduler executor async-aware for notification.

**Why keep `output_summary` alongside `output`?**
`output_summary` is used in `schedule_search` results and notification frames. Sending full (potentially huge) output over WebSocket just for a notification toast would be wasteful.

---

## Verification

1. **Build**: `cargo build` — ensure all crates compile
2. **Tests**: `cargo test` — existing scheduler tests pass (update `MockNotifier` for new `notify_user` signature)
3. **Manual Telegram test**: Create a schedule from Telegram → wait for it to fire → verify message arrives in the same chat/thread
4. **Manual TUI test**: Create a schedule from TUI → verify notification still broadcasts to connected TUI sessions
5. **History test**: After a scheduled run completes, ask the agent to show schedule run history → verify full output is returned via `schedule_runs` tool
