# Plan: Robust Browser Automation — Hybrid Tool + Skill Architecture

> **Status:** Proposed
> **Created:** 2026-03-04

---

## 1. Problem Statement

Oxydra needs reliable, performant, multi-agent browser automation. The system
architecture has two containers: **oxydra-vm** (agent runtime, LLM tool
dispatch) and **shell-vm** (shell-daemon + Pinchtab Chrome bridge). All shell
commands travel this path:

```
LLM tool call → oxydra-vm → Unix socket (length-delimited JSON)
  → shell-daemon → sh -lc "curl ..." → Pinchtab (127.0.0.1 TCP)
  → response back through the same chain
```

Each `shell_exec` invocation requires at minimum 4 RPC round-trips over the
Unix socket (ExecCommand + ExecCommandAck + N×StreamOutput + final EOF chunk).
For browser automation, the LLM currently issues 4–8 `shell_exec` calls per
browser operation (list tabs, lock, navigate, sleep, snapshot, act, unlock),
multiplying the transport overhead.

### Current Failure Modes

1. **No guaranteed cleanup** — If the LLM's turn fails mid-task, orphaned tab
   locks persist until TTL expiry (up to 10 minutes by default).
2. **Fragile agent identity** — LLMs must self-generate UUIDs via
   `/proc/sys/kernel/random/uuid` and track them across commands. They often
   regenerate, orphaning previous locks.
3. **Race conditions** — Between creating a tab and locking it, another agent
   can steal it. No atomic create-and-lock exists in Pinchtab.
4. **Transport latency** — Each curl command is a separate `shell_exec` call
   with full Unix-socket RPC overhead (observed: multi-second delays on some
   operations).
5. **Discoverability regression** — Critical endpoints (screenshot, PDF,
   download, upload, evaluate) were removed from the SKILL.md body.

### Goal

A browser automation system that:
- Guarantees tab lock cleanup on success, failure, and crash
- Derives stable agent identity without LLM cooperation
- Minimizes transport overhead by batching operations
- Provides the LLM with a clean, simple interface
- Handles all concurrency edge cases (409 conflicts, capacity limits, TTL
  expiry) without LLM involvement

---

## 2. Research Findings

### 2.1 Pinchtab API — Ground Truth

Verified directly from Pinchtab's Go source code (v0.7.6):

**Both flat and resource-based paths are registered and work.** The
resource-based paths (`/tabs/{id}/...`) are thin adapters that extract the tab
ID from the URL path and delegate to the same handler as the flat paths.

| Operation | Flat Path | Resource Path | Preferred |
|-----------|-----------|---------------|-----------|
| Navigate | `POST /navigate` | `POST /tabs/{id}/navigate` | Flat |
| Snapshot | `GET /snapshot` | `GET /tabs/{id}/snapshot` | Flat |
| Action | `POST /action` | `POST /tabs/{id}/action` | Flat |
| Batch actions | `POST /actions` | `POST /tabs/{id}/actions` | Flat |
| Text | `GET /text` | `GET /tabs/{id}/text` | Flat |
| Screenshot | `GET /screenshot` | `GET /tabs/{id}/screenshot` | Flat |
| PDF | `GET /pdf` | `GET /tabs/{id}/pdf` | Flat |
| Evaluate | `POST /evaluate` | `POST /tabs/{id}/evaluate` | Flat |
| List tabs | `GET /tabs` | — | Flat |
| Open/close | `POST /tab` | — | Flat |
| Lock | `POST /tab/lock` | `POST /tabs/{id}/lock` | Flat |
| Unlock | `POST /tab/unlock` | `POST /tabs/{id}/unlock` | Flat |
| Cookies | `GET/POST /cookies` | `GET/POST /tabs/{id}/cookies` | Flat |
| Download | `GET /download` | — | Flat |
| Upload | `POST /upload` | — | Flat |
| Health | `GET /health` | — | Flat |
| Stealth | `GET /stealth/status` | — | Flat |
| Fingerprint | `POST /fingerprint/rotate` | `POST /tabs/{id}/fingerprint/rotate` | Flat |

**Decision:** Use flat paths. They match the official skill reference
(`skill/pinchtab/references/api.md`), require no path construction, and work
with tabId in query params or POST body.

### 2.2 API Details (Verified from Go Source)

**`GET /tabs` response** (from `HandleTabs` in `health_tabs.go`):
```json
{"tabs": [{"id": "...", "url": "...", "title": "...", "type": "page"}]}
```
Wrapped object with `tabs` key — NOT a flat array. Locked tabs include
`owner` and `lockedUntil` fields.

**Lock request body** (from `HandleTabLock` in `lock_shutdown.go`):
```json
{"tabId": "...", "owner": "...", "timeoutSec": 60}
```
- Field name is **`timeoutSec`** (not `ttl` — pinchtab.com docs are wrong)
- Default timeout: **10 minutes** (`DefaultLockTimeout = 10 * time.Minute`)
- Max timeout: Not hard-capped in server code (unlike earlier docs claiming 5 min)
- Re-locking by same owner is allowed (extends the TTL)
- 409 Conflict if locked by different owner

**Snapshot query params** (from official skill):
- `filter=interactive` — only interactive elements (NOT `interactive=true`)
- `format=compact` — most token-efficient format (NOT `compact=true`)
- `diff=true` — only changes since last snapshot
- `maxTokens=2000` — hard truncation
- `selector=main` — CSS selector scope
- `depth=N` — tree depth limit

### 2.3 Transport Architecture

The command execution chain:

```
oxydra-vm tool call
  ↓ (in-process)
execute_with_shell_session()
  ↓ ExecCommand (length-delimited JSON, Unix socket)
shell-daemon: SessionManager.exec_command()
  ↓ tokio::process::Command("sh", "-lc", command)
sh: curl -s http://127.0.0.1:9867/...
  ↓ TCP loopback
Pinchtab: HandleXxx()
  ↓ response
shell-daemon: output_queue (VecDeque, 8KB chunks, 64KB max)
  ↓ StreamOutput RPC (may be multiple chunks)
execute_with_shell_session(): concatenate stdout/stderr
  ↓ return to tool
LLM receives tool result
```

**Key bottleneck:** Each `shell_exec` call involves:
1. ExecCommand request (JSON serialize → length-prefix → socket write)
2. ExecCommandAck response (socket read → deserialize)
3. 1+ StreamOutput requests (one per 8KB chunk)
4. StreamOutputChunk responses with `eof: true` termination

For a typical snapshot response (~4KB), this is 4 RPC messages minimum. For a
full browser operation (list + lock + navigate + sleep 3 + snapshot + act +
diff + unlock), that's **8 separate `shell_exec` invocations × 4+ RPCs each
= 32+ Unix socket messages**, plus the LLM processing time between each call.

**The fix:** Batch multiple curl commands into a single `shell_exec` call.

### 2.4 Pinchtab's OpenClaw Plugin Pattern

Pinchtab ships a TypeScript OpenClaw (MCP) plugin (`plugin/index.ts`, ~260
lines) that wraps the REST API as a single tool with action-based dispatch:

```
tool: "pinchtab"
  action: navigate | snapshot | click | type | press | fill | hover |
          scroll | select | focus | text | tabs | screenshot |
          evaluate | pdf | health
```

Key design decisions:
- **Single tool** — minimizes schema tokens in the system prompt
- **Action enum** — one `action` field routes to the right endpoint
- **No locking exposed** — the plugin doesn't surface tab locking at all
- **Stateless** — every call is independent, no session tracking
- **Error normalization** — HTTP errors → `{error: "..."}` text

The plugin does NOT handle: locking, session persistence, crash recovery,
stealth, resource blocking. These are all server-side concerns.

### 2.5 Session Identity Flow

```
User message → Channel (TUI/Telegram)
  → Gateway: resolve/create session_id (UUID v7)
  → RuntimeGatewayTurnRunner.run_turn(user_id, session_id, ...)
  → ToolExecutionContext { session_id: Some("..."), user_id: Some("..."), ... }
  → Tool.execute(args, &context)
```

`session_id` is available in `ToolExecutionContext` at tool execution time.
It is stable across turns within the same session and unique across sessions.
It is NOT currently passed to the shell-vm container — it lives only in the
oxydra-vm process.

A dedicated Rust tool can read `context.session_id` directly and use it as
the lock owner. No env var plumbing needed.

---

## 3. Architecture Decision: Hybrid Tool + Skill

### Why Not Skill-Only

| Problem | Skill-only mitigation | Adequacy |
|---------|----------------------|----------|
| Agent identity | `cat /proc/sys/kernel/random/uuid` | ❌ Fragile |
| Guaranteed cleanup | "ALWAYS unlock" in instructions | ❌ LLM crashes/timeouts |
| Race in create-then-lock | "Try next tab" in instructions | ❌ LLM doesn't always follow |
| Transport overhead | N/A | ❌ 8 shell_exec calls per op |
| Lock renewal | "Re-lock before expiry" | ❌ LLM won't spontaneously do this |

### Why Not Full Dedicated Tool (2000+ LOC SDK)

The original plan doc correctly identifies the maintenance burden:
- Every Pinchtab API change requires Rust code changes + recompilation
- Provider-specific JSON schema quirks (Gemini `items` requirement, etc.)
- Would need to model 16+ action types with per-action parameters

### Hybrid: Thin Lifecycle Tool + Skill for Secondary Operations

A **~300-line Rust tool** that handles only the **lifecycle concerns** the
LLM cannot reliably manage:

| Concern | Handled by | Mechanism |
|---------|-----------|-----------|
| Agent identity | Tool (Rust) | `context.session_id` from ToolExecutionContext |
| Tab acquisition | Tool (Rust) | List → Lock with retry, atomic within one call |
| Lock cleanup | Tool (Rust) | Always-unlock via `trap EXIT` in generated script |
| Lock renewal | Tool (Rust) | Re-lock before each operation in generated script |
| Transport batching | Tool (Rust) | Multiple curl commands in one shell_exec |
| Navigate + wait | Tool (Rust) | Includes `sleep 3` in the batch script |
| Snapshot/Act/Text | Tool (Rust) | Included in batch, returns result directly |
| PDF/Screenshot/Download | Skill (SKILL.md) | LLM uses shell_exec with curl patterns |
| Upload/Evaluate/Cookies | Skill (SKILL.md) | LLM uses shell_exec with curl patterns |
| Stealth/Fingerprint | Skill (SKILL.md) | LLM uses shell_exec with curl patterns |

The tool handles the **hot path** (navigate → snapshot → act → diff) in a
single tool call. The skill teaches **secondary operations** that don't need
lifecycle management.

---

## 4. Detailed Design

### 4.1 Browser Tool (`crates/tools/src/browser.rs`)

#### Tool Schema

```json
{
  "name": "browser",
  "description": "Control a headless Chrome browser. Automatically acquires/locks a tab, performs the operation, and releases the lock.",
  "parameters": {
    "type": "object",
    "required": ["action"],
    "properties": {
      "action": {
        "type": "string",
        "enum": ["navigate", "snapshot", "act", "text", "screenshot", "tabs", "health"],
        "description": "The browser operation to perform"
      },
      "url": {
        "type": "string",
        "description": "URL to navigate to (for navigate action)"
      },
      "tabId": {
        "type": "string",
        "description": "Target tab ID. If omitted, auto-acquired."
      },
      "ref": {
        "type": "string",
        "description": "Element reference to act on (e.g. e5)"
      },
      "kind": {
        "type": "string",
        "enum": ["click", "type", "press", "fill", "hover", "scroll", "select", "focus"],
        "description": "Action kind (for act action)"
      },
      "text": {
        "type": "string",
        "description": "Text to type or fill"
      },
      "key": {
        "type": "string",
        "description": "Key to press (for act with kind=press)"
      },
      "value": {
        "type": "string",
        "description": "Value to select (for act with kind=select)"
      },
      "selector": {
        "type": "string",
        "description": "CSS selector for snapshot scoping or fill target"
      },
      "diff": {
        "type": "boolean",
        "description": "Return only changes since last snapshot"
      },
      "waitNav": {
        "type": "boolean",
        "description": "Wait for navigation after click"
      }
    }
  }
}
```

Single tool, ~7 actions covering the hot path. The LLM calls it with
structured parameters; the tool handles locking, batching, and cleanup.

#### Internal Execution Flow

```
browser tool execute(args, context)
  │
  ├─ owner = context.session_id.unwrap_or(generate_uuid())
  │
  ├─ match action:
  │   ├─ "navigate" → build_navigate_script(owner, url)
  │   ├─ "snapshot" → build_snapshot_script(owner, tabId, diff, selector)
  │   ├─ "act"      → build_act_script(owner, tabId, kind, ref, text, ...)
  │   ├─ "text"     → build_text_script(owner, tabId)
  │   ├─ "screenshot" → build_screenshot_script(owner, tabId)
  │   ├─ "tabs"     → build_tabs_script()  (no locking needed)
  │   └─ "health"   → build_health_script() (no locking needed)
  │
  ├─ shell_session.exec_command(script)  ← ONE shell_exec call
  │
  └─ parse and return result
```

#### Tab Acquisition + Cleanup Script (generated by tool)

For actions that need a tab, the tool generates a bash script like:

```bash
#!/bin/sh
set -e
BASE="http://127.0.0.1:9867"
AUTH="Authorization: Bearer $BRIDGE_TOKEN"
OWNER="<session_id from ToolExecutionContext>"

# === Tab acquisition ===
if [ -n "$REQUESTED_TAB" ]; then
  # Lock the specific requested tab
  curl -sf -X POST "$BASE/tab/lock" \
    -H "$AUTH" -H 'Content-Type: application/json' \
    -d "{\"tabId\":\"$REQUESTED_TAB\",\"owner\":\"$OWNER\",\"timeoutSec\":120}" \
    > /dev/null 2>&1 || true
  TAB="$REQUESTED_TAB"
else
  # List tabs and find an unlocked one
  TABS=$(curl -sf "$BASE/tabs" -H "$AUTH")
  TAB=$(echo "$TABS" | jq -r '.tabs[] | select(.owner == null) | .id' | head -1)

  if [ -z "$TAB" ]; then
    TOTAL=$(echo "$TABS" | jq '.tabs | length')
    if [ "$TOTAL" -lt 20 ]; then
      TAB=$(curl -sf -X POST "$BASE/tab" \
        -H "$AUTH" -H 'Content-Type: application/json' \
        -d '{"action":"new"}' | jq -r '.tabId')
    else
      echo '{"error":"all 20 tabs are locked, retry later"}'
      exit 0
    fi
  fi

  # Lock the tab (retry with new tab on 409)
  curl -sf -X POST "$BASE/tab/lock" \
    -H "$AUTH" -H 'Content-Type: application/json' \
    -d "{\"tabId\":\"$TAB\",\"owner\":\"$OWNER\",\"timeoutSec\":120}" \
    > /dev/null 2>&1 || {
    TAB=$(curl -sf -X POST "$BASE/tab" \
      -H "$AUTH" -H 'Content-Type: application/json' \
      -d '{"action":"new"}' | jq -r '.tabId')
    curl -sf -X POST "$BASE/tab/lock" \
      -H "$AUTH" -H 'Content-Type: application/json' \
      -d "{\"tabId\":\"$TAB\",\"owner\":\"$OWNER\",\"timeoutSec\":120}" \
      > /dev/null 2>&1 || true
  }
fi

# === Guaranteed cleanup ===
unlock() {
  curl -sf -X POST "$BASE/tab/unlock" \
    -H "$AUTH" -H 'Content-Type: application/json' \
    -d "{\"tabId\":\"$TAB\",\"owner\":\"$OWNER\"}" \
    > /dev/null 2>&1 || true
}
trap unlock EXIT

# === Operation (injected per action type) ===
<navigate / snapshot / act / text commands here>

# trap fires on exit → unlock always runs
```

The `trap unlock EXIT` ensures cleanup even if the script fails, is killed,
or times out. This is the critical improvement over the skill-only approach.

#### Navigate Action (Example Generated Script Body)

```bash
# ... tab acquisition + trap from above ...

# Navigate
curl -sf -X POST "$BASE/navigate" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"url\":\"$URL\",\"tabId\":\"$TAB\"}"

# Wait for accessibility tree
sleep 3

# Auto-snapshot after navigation (token-efficient defaults)
echo "---SNAPSHOT---"
curl -sf "$BASE/snapshot?tabId=$TAB&format=compact&filter=interactive&maxTokens=2000" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
```

Navigate automatically includes the 3-second wait and returns a snapshot.
This collapses what was previously 4+ separate `shell_exec` calls into 1.

#### Act Action (Example Generated Script Body)

```bash
# ... tab acquisition + trap ...

# Renew lock (idempotent with same owner)
curl -sf -X POST "$BASE/tab/lock" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"tabId\":\"$TAB\",\"owner\":\"$OWNER\",\"timeoutSec\":120}" \
  > /dev/null 2>&1 || true

# Perform action
curl -sf -X POST "$BASE/action" \
  -H "$AUTH" -H 'Content-Type: application/json' \
  -d "{\"kind\":\"$KIND\",\"ref\":\"$REF\",\"tabId\":\"$TAB\"$EXTRA}"

# Auto diff-snapshot after action
sleep 1
echo "---SNAPSHOT---"
curl -sf "$BASE/snapshot?tabId=$TAB&format=compact&filter=interactive&diff=true" \
  -H "$AUTH"
echo ""
echo "---TAB_ID---"
echo "$TAB"
```

Each act call renews the lock and returns a diff snapshot, eliminating
the need for the LLM to manage lock renewal or request snapshots.

### 4.2 Tool Registration and Activation

```rust
// In crates/tools/src/lib.rs, during tool registration
if browser_available {
    registry.register(Box::new(BrowserTool::new(
        pinchtab_url.clone(),
        shell_session.clone(),
    )));
}
```

The tool needs:
- `pinchtab_url` — from `BrowserToolConfig.pinchtab_base_url`
- `shell_session` — the existing shell session handle (same as `shell_exec`)

It does NOT need a direct HTTP client. It delegates all HTTP calls to curl
via the shell session, maintaining the existing security boundary (all
network access stays inside shell-vm).

### 4.3 Updated SKILL.md

The SKILL.md teaches the `browser` tool for the hot path and curl for
secondary operations:

```markdown
---
name: browser-automation
description: Control a headless Chrome browser via Pinchtab's REST API
activation: auto
requires:
  - shell_exec
env:
  - PINCHTAB_URL
priority: 50
---

## Browser Automation

Use the `browser` tool for common operations. It handles tab locking,
cleanup, and accessibility tree wait times automatically.

### Quick Start

1. **Navigate** (auto-acquires tab, waits 3s, returns snapshot):
   `browser(action="navigate", url="https://example.com")`

2. **Act** on a ref from the snapshot:
   `browser(action="act", kind="click", ref="e5", tabId="<from step 1>")`

3. **Read text** (~800 tokens vs ~10K for full snapshot):
   `browser(action="text", tabId="...")`

4. **Diff snapshot** (only changes, ~90% fewer tokens):
   `browser(action="snapshot", tabId="...", diff=true)`

5. **List tabs** (see lock status):
   `browser(action="tabs")`

### Tab Management

- The tool acquires and locks a tab automatically on each call
- Pass `tabId` from a previous result to reuse the same tab
- The tool unlocks the tab when the operation completes (even on failure)
- **20-tab system limit** — the tool handles capacity automatically
- Locks auto-expire after timeout; if your tab is stolen, the tool
  re-acquires from available tabs

### Action Kinds

For `action="act"`, use these `kind` values:
- `click` — click element by ref (add `waitNav=true` if it triggers navigation)
- `type` — type text into element (set `text`)
- `press` — press a key (set `key`, e.g. "Enter")
- `fill` — set value directly (set `text` and optionally `selector`)
- `hover` — hover over element (triggers dropdowns/tooltips)
- `scroll` — scroll to element by ref
- `select` — select dropdown option (set `value`)
- `focus` — focus an element

### File Operations (use shell_exec + curl)

Save screenshots:
  `curl "{{PINCHTAB_URL}}/screenshot?tabId=$TAB&raw=true" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" -o /shared/screenshot.png`

Export PDF:
  `curl "{{PINCHTAB_URL}}/pdf?tabId=$TAB&output=file&path=/shared/.pinchtab/page.pdf" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" && \
    cp /shared/.pinchtab/page.pdf /shared/page.pdf`

Download file:
  `curl "{{PINCHTAB_URL}}/download?url=URL&output=file&path=/shared/.pinchtab/f.ext" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" && \
    cp /shared/.pinchtab/f.ext /shared/f.ext`

Upload file:
  `curl -X POST "{{PINCHTAB_URL}}/upload" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"selector":"input[type=file]","paths":["/shared/photo.jpg"],"tabId":"ID"}'`

Evaluate JS:
  `curl -X POST "{{PINCHTAB_URL}}/evaluate" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"expression":"document.title","tabId":"ID"}'`

Set cookies:
  `curl -X POST "{{PINCHTAB_URL}}/cookies" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' \
    -d '{"url":"https://example.com","cookies":[{"name":"s","value":"v"}]}'`

Stealth/fingerprint:
  `curl "{{PINCHTAB_URL}}/stealth/status" -H "Authorization: Bearer $BRIDGE_TOKEN"`
  `curl -X POST "{{PINCHTAB_URL}}/fingerprint/rotate" \
    -H "Authorization: Bearer $BRIDGE_TOKEN" \
    -H 'Content-Type: application/json' -d '{"os":"windows"}'`

After saving files to /shared/, use `send_media` to deliver to the user.

### Error Handling

- Tab capacity full → tool returns error, retry after a few seconds
- CAPTCHAs / 2FA / login walls → call `request_human_assistance`

Full API reference:
`cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
```

**Token estimate:** ~950 tokens — well within the 3,000 cap. The tool schema
adds ~200 tokens to the system prompt (via tool registration), for a total
of ~1,150 tokens.

### 4.4 Updated references/pinchtab-api.md

The reference file should contain:
1. All flat-path endpoints with auth headers and curl examples
2. Tab locking section (`/tab/lock`, `/tab/unlock`, `timeoutSec` field name,
   `GET /tabs` response showing `owner` and `lockedUntil`)
3. Correct response formats (e.g., `GET /tabs` returns `{"tabs": [...]}`)
4. Stealth section (`/stealth/status`, `/fingerprint/rotate`)
5. All action kinds with examples: click, type, press, fill, hover, scroll,
   select, focus (including `waitNav` on click)
6. Snapshot params: `format=compact`, `filter=interactive`, `diff=true`,
   `maxTokens`, `selector`, `depth`, `noAnimations`
7. PDF params: `landscape`, `scale`, `margins`, `pageRanges`,
   `displayHeaderFooter`, `output=file`, `path`, `raw`
8. Download, upload, evaluate, cookies, screenshot sections

### 4.5 Infrastructure Changes

#### Runner: BRIDGE_MAX_TABS and BRIDGE_NO_RESTORE

**File:** `crates/runner/src/lib.rs` → `build_browser_env()`

Add `BRIDGE_MAX_TABS=20` and `BRIDGE_NO_RESTORE=true` to the env vars.

#### No AGENT_ID Env Var Needed

The `browser` tool reads `context.session_id` directly from
`ToolExecutionContext` (populated by the gateway). No env var plumbing into
shell-vm is required. The identity stays in Rust, never in the shell.

---

## 5. Performance Analysis

### Transport Overhead Comparison

**Skill-only (navigate + snapshot):**
```
shell_exec("curl .../tabs")           → 4+ RPC messages
shell_exec("curl .../tab/lock")       → 4+ RPC messages
shell_exec("curl .../navigate")       → 4+ RPC messages
shell_exec("sleep 3")                 → 4+ RPC messages
shell_exec("curl .../snapshot")       → 4+ RPC messages
shell_exec("curl .../tab/unlock")     → 4+ RPC messages
Total: 6 shell_exec calls, 24+ RPCs, 6 LLM processing gaps
```

**Hybrid tool (navigate + snapshot):**
```
browser(action="navigate", url="...")  → 1 tool call
  → 1 shell_exec with batched script  → 4+ RPC messages
Total: 1 tool call, 4+ RPCs, 0 LLM processing gaps
```

**Result:** 6× fewer RPC round-trips, zero inter-call LLM latency.

### Token Overhead Comparison

**Skill-only:** ~250 output tokens per browser operation (6 curl commands).
**Hybrid tool:** ~30 output tokens per browser operation (1 structured call).

**Result:** ~88% fewer output tokens per operation.

---

## 6. Implementation Plan

### Phase 1: Infrastructure

**Files:**
- `crates/runner/src/lib.rs` — Add `BRIDGE_MAX_TABS=20`,
  `BRIDGE_NO_RESTORE=true` to `build_browser_env()`
- `crates/runner/src/tests.rs` — Update env var assertions

**Verify:** `cargo test -p runner -- build_browser_env`

### Phase 2: Browser Tool

**Files:**
- `crates/tools/src/browser.rs` — Create `BrowserTool` (~300 lines)
- `crates/tools/src/lib.rs` — Register tool when browser is available

**Architecture:**

```rust
pub struct BrowserTool {
    pinchtab_url: String,
    session: Arc<Mutex<Box<dyn ShellSession>>>,
}

#[async_trait]
impl Tool for BrowserTool {
    fn schema(&self) -> FunctionDecl { /* single-tool schema */ }

    async fn execute(
        &self, args: &str, context: &ToolExecutionContext,
    ) -> Result<String, ToolError> {
        let params: BrowserParams = serde_json::from_str(args)?;
        let owner = context.session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let script = match params.action.as_str() {
            "navigate"   => self.build_navigate_script(&owner, &params),
            "snapshot"   => self.build_snapshot_script(&owner, &params),
            "act"        => self.build_act_script(&owner, &params),
            "text"       => self.build_text_script(&owner, &params),
            "screenshot" => self.build_screenshot_script(&owner, &params),
            "tabs"       => self.build_tabs_script(),
            "health"     => self.build_health_script(),
            _ => return Err(invalid_params("unknown action")),
        };

        execute_with_shell_session(&self.session, &script, timeout).await
    }
}
```

**Note:** `execute_with_shell_session()` in `lib.rs` is currently private.
Either make it `pub(crate)` so `browser.rs` can call it, or have
`BrowserTool` use the `ShellSession` trait methods directly (`exec_command`
+ `stream_output` loop).

**Verify:** Unit tests for script generation + integration tests with mock shell

### Phase 3: Skill and Reference Updates

**Files:**
- `config/skills/BrowserAutomation/SKILL.md` — Rewrite per Section 4.3
- `config/skills/BrowserAutomation/references/pinchtab-api.md` — Update per
  Section 4.4

**Verify:** Skill parsing tests, token cap test, content validation tests

### Phase 4: Tests

**New tests:**
1. `navigate_script_includes_lock_and_trap_exit` — script has lock + `trap`
2. `act_script_includes_lock_renewal` — re-locks before action
3. `tabs_script_has_no_locking` — list-tabs skips locking
4. `owner_derived_from_session_id` — uses context.session_id
5. `owner_fallback_when_no_session_id` — generates UUID
6. `navigate_script_uses_correct_snapshot_params` — format=compact, etc.
7. Updated skill content tests for new SKILL.md structure
8. Updated runner config tests for BRIDGE_MAX_TABS=20

**Verify:**
```
cargo fmt --all
cargo clippy -p tools -p runner --all-targets --all-features -- -D warnings
cargo test -p tools -p runner -- --test-threads=1
```

### Phase 5: Manual Validation

1. `browser(action="navigate", url="https://example.com")` → snapshot
2. `browser(action="act", kind="click", ref="e5", tabId="...")` → diff
3. `browser(action="text", tabId="...")` → page text
4. `browser(action="tabs")` → verify unlock after each call
5. Two concurrent sessions → each gets different tab, no 409s
6. Kill shell mid-execution → lock expires after TTL

---

## 7. Detailed Change List

| # | File | Type | Description |
|---|------|------|-------------|
| 1 | `crates/tools/src/browser.rs` | Create | BrowserTool implementation (~300 lines) |
| 2 | `crates/tools/src/lib.rs` | Edit | Register BrowserTool when browser available |
| 3 | `crates/runner/src/lib.rs` | Edit | Add BRIDGE_MAX_TABS=20, BRIDGE_NO_RESTORE=true |
| 4 | `config/skills/BrowserAutomation/SKILL.md` | Rewrite | Tool-first + secondary curl patterns |
| 5 | `config/skills/BrowserAutomation/references/pinchtab-api.md` | Update | Correct params, add locking/stealth |
| 6 | `crates/runner/src/skills.rs` | Edit | Update content-validation tests |
| 7 | `crates/runner/src/tests.rs` | Edit | Update build_browser_env test |

### Files NOT Changed

| File | Reason |
|------|--------|
| `docker/shell-vm-entrypoint.sh` | Already correct for manager mode |
| `docker/chromium-wrapper.sh` | Unaffected by tool changes |
| `crates/types/src/runner.rs` | No new fields; session_id already in ToolExecutionContext |
| `crates/types/src/tool.rs` | ToolExecutionContext already has session_id |
| `crates/gateway/src/turn_runner.rs` | Already populates session_id |

---

## 8. Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|------------|
| Shell session unavailable | Tool returns error | ToolError with message; SKILL.md curl is backup |
| Pinchtab API changes break lock/tabs | Lock fails | Lock code is ~30 lines; easy to update |
| Generated script has bash errors | Silent failure | Unit tests validate scripts; integration tests run them |
| session_id is None | Random UUID owner | Functionally correct, just not stable across turns |
| Task exceeds 120s lock TTL | Lock expires | Act action renews lock; server default is 10min |
| Pre-warm tab has stale content | about:blank tab | Navigate always replaces URL |
| 64KB shell output limit | Truncated snapshot | maxTokens=2000 keeps output well under 64KB |
| Provider schema quirks | Tool rejected | Schema uses only string/boolean/enum; no arrays |

---

## 9. Future Considerations

### Direct HTTP from Tool (Bypass Shell)

If transport latency remains a concern after batching, the tool could make
HTTP requests directly from oxydra-vm to Pinchtab (which binds to a port
accessible from both containers). This eliminates the Unix socket overhead
entirely but requires an HTTP client dependency in `crates/tools`.

**Trigger:** Evidence of >2s overhead per batched operation.

### Shell-Daemon Protocol Extension

Add `HttpRequest`/`HttpResponse` message types to the shell-daemon protocol.
The daemon performs HTTP calls directly (no shell fork, no curl spawn) and
returns structured responses. Keeps the security boundary while eliminating
curl/jq process overhead.

**Trigger:** Need for sub-second browser operations.

### Tab Lease Across Tool Calls

Currently each tool call acquires and releases a tab. For multi-step
workflows, the LLM passes `tabId` to reuse the same tab but the lock is
released between calls. A session-scoped lease could keep a tab locked
across multiple tool calls within the same turn, with cleanup at turn end.

**Trigger:** Lock-acquisition overhead dominating multi-step workflows.

---

## Appendix A: Verified API Ground Truth

> **Source:** Pinchtab Go source code (v0.7.6), verified from `HandleTabs`,
> `HandleTabLock`, `HandleNavigate`, `HandleSnapshot`, `HandleAction`, and
> the official GitHub `skill/pinchtab/references/api.md`.
>
> **WARNING:** The root-level `pinchtab-api.md` in this repo was scraped from
> pinchtab.com and contains **incorrect** paths (`/tabs/{id}/lock`),
> field names (`ttl` instead of `timeoutSec`), and response formats (flat
> array instead of wrapped object). **Do not use it as a reference for
> implementation.** Use only this appendix and the official GitHub api.md.

### A.1 Tab Listing

```bash
curl -s http://127.0.0.1:9867/tabs \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
```

**Response** (wrapped object, NOT flat array):
```json
{
  "tabs": [
    {
      "id": "tab_abc12345",
      "url": "https://example.com",
      "title": "Example",
      "type": "page"
    },
    {
      "id": "tab_def67890",
      "url": "https://google.com",
      "title": "Google",
      "type": "page",
      "owner": "session-uuid-here",
      "lockedUntil": "2026-03-04T12:05:00Z"
    }
  ]
}
```

`owner` and `lockedUntil` are only present on locked tabs.

### A.2 Tab Create / Close

```bash
# Create new tab
curl -s -X POST http://127.0.0.1:9867/tab \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"action": "new", "url": "https://example.com"}'
# Response: {"tabId": "tab_abc12345", ...}

# Create blank tab (url optional)
curl -s -X POST http://127.0.0.1:9867/tab \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"action": "new"}'

# Close tab
curl -s -X POST http://127.0.0.1:9867/tab \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"action": "close", "tabId": "tab_abc12345"}'
```

### A.3 Tab Locking

```bash
# Lock a tab
curl -s -X POST http://127.0.0.1:9867/tab/lock \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"tabId": "tab_abc12345", "owner": "my-session-id", "timeoutSec": 120}'
# Success response: {"tabId":"tab_abc12345","owner":"my-session-id","lockedUntil":"..."}
# Conflict (409): {"error":"tab tab_abc12345 is locked by other-owner for another 9m30s"}

# Unlock a tab (owner must match)
curl -s -X POST http://127.0.0.1:9867/tab/unlock \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"tabId": "tab_abc12345", "owner": "my-session-id"}'

# Renew lock (re-lock with same owner is allowed, extends TTL)
curl -s -X POST http://127.0.0.1:9867/tab/lock \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"tabId": "tab_abc12345", "owner": "my-session-id", "timeoutSec": 120}'
```

**Critical field names:**
- `timeoutSec` (NOT `ttl`, NOT `timeout`, NOT `timeoutSeconds`)
- `tabId` (NOT `tab_id`, NOT `id`)
- `owner` (free-form string, used for identity matching)

**Defaults:**
- `timeoutSec` omitted or 0 → server default of **10 minutes**
- No hard max cap in server code

### A.4 Navigate

```bash
curl -s -X POST http://127.0.0.1:9867/navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com", "tabId": "tab_abc12345"}'
# Response: {"tabId":"tab_abc12345","title":"Example","url":"https://example.com/"}

# Navigate in NEW tab (creates tab, returns its ID)
curl -s -X POST http://127.0.0.1:9867/navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url": "https://example.com", "newTab": true}'
# Response: {"tabId":"tab_NEW_ID","title":"Example","url":"https://example.com/"}

# Optional params: timeout (seconds), blockImages (bool)
```

**Important:** Without `tabId`, navigates the active (most recently used)
tab. Without `newTab: true`, no new tab is created.

### A.5 Snapshot

```bash
# Token-efficient defaults (recommended)
curl -s "http://127.0.0.1:9867/snapshot?tabId=TAB&format=compact&filter=interactive&maxTokens=2000" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Diff snapshot (only changes since last snapshot for this tab)
curl -s "http://127.0.0.1:9867/snapshot?tabId=TAB&format=compact&filter=interactive&diff=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
```

**Query params** (all optional):
- `tabId` — target tab (omit for active tab)
- `format=compact` — most token-efficient (NOT `compact=true`)
- `filter=interactive` — only interactive elements (NOT `interactive=true`)
- `diff=true` — only changes since last snapshot
- `maxTokens=2000` — truncate output
- `selector=main` — CSS selector to scope the tree
- `depth=N` — max tree depth
- `noAnimations=true` — disable CSS animations before capture

**Response:** Flat JSON array of nodes with `ref`, `role`, `name`, etc.

### A.6 Actions

```bash
# Click
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "click", "ref": "e5", "tabId": "TAB"}'

# Click and wait for navigation
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "click", "ref": "e5", "tabId": "TAB", "waitNav": true}'

# Type text
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "type", "ref": "e12", "text": "hello", "tabId": "TAB"}'

# Press key
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "press", "key": "Enter", "tabId": "TAB"}'

# Fill (set value directly, no keystrokes)
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "fill", "selector": "#email", "text": "user@example.com", "tabId": "TAB"}'

# Hover
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "hover", "ref": "e8", "tabId": "TAB"}'

# Select dropdown
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "select", "ref": "e10", "value": "option2", "tabId": "TAB"}'

# Scroll by pixels
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "scroll", "scrollY": 800, "tabId": "TAB"}'

# Scroll to element
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "scroll", "ref": "e20", "tabId": "TAB"}'

# Focus
curl -s -X POST http://127.0.0.1:9867/action \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind": "focus", "ref": "e3", "tabId": "TAB"}'
```

**Batch actions:**
```bash
curl -s -X POST http://127.0.0.1:9867/actions \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"actions":[{"kind":"click","ref":"e3"},{"kind":"type","ref":"e3","text":"hello"},{"kind":"press","key":"Enter"}],"stopOnError":true,"tabId":"TAB"}'
```

### A.7 Text Extraction

```bash
# Readability mode (strips nav/footer/ads)
curl -s "http://127.0.0.1:9867/text?tabId=TAB" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Raw innerText
curl -s "http://127.0.0.1:9867/text?tabId=TAB&mode=raw" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
```

**Response:** `{"url": "...", "title": "...", "text": "..."}`

### A.8 Screenshot

```bash
curl -s "http://127.0.0.1:9867/screenshot?tabId=TAB&raw=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o /shared/screenshot.png

# JPEG with quality
curl -s "http://127.0.0.1:9867/screenshot?tabId=TAB&raw=true&quality=50" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o /shared/screenshot.jpg
```

### A.9 PDF Export

```bash
# Save to disk (path must be under /shared/.pinchtab/)
curl -s "http://127.0.0.1:9867/pdf?tabId=TAB&output=file&path=/shared/.pinchtab/page.pdf" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Raw PDF bytes
curl -s "http://127.0.0.1:9867/pdf?tabId=TAB&raw=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o page.pdf

# Landscape with custom scale
curl -s "http://127.0.0.1:9867/pdf?tabId=TAB&landscape=true&scale=0.8&raw=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" -o page.pdf
```

**Params:** `paperWidth`, `paperHeight`, `landscape`, `marginTop/Bottom/Left/Right`,
`scale` (0.1–2.0), `pageRanges`, `displayHeaderFooter`, `headerTemplate`,
`footerTemplate`, `preferCSSPageSize`, `output` (file/JSON), `path`, `raw`.

### A.10 Download / Upload

```bash
# Download to disk
curl -s "http://127.0.0.1:9867/download?url=https://site.com/file.csv&output=file&path=/shared/.pinchtab/file.csv" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Upload
curl -s -X POST http://127.0.0.1:9867/upload \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"selector": "input[type=file]", "paths": ["/shared/photo.jpg"], "tabId": "TAB"}'
```

### A.11 Evaluate JavaScript

```bash
curl -s -X POST http://127.0.0.1:9867/evaluate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"expression": "document.title", "tabId": "TAB"}'
```

### A.12 Cookies

```bash
# Get cookies
curl -s "http://127.0.0.1:9867/cookies?tabId=TAB" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Set cookies
curl -s -X POST http://127.0.0.1:9867/cookies \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","cookies":[{"name":"session","value":"abc123"}]}'
```

### A.13 Stealth / Fingerprint

```bash
# Check stealth status
curl -s http://127.0.0.1:9867/stealth/status \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Rotate fingerprint
curl -s -X POST http://127.0.0.1:9867/fingerprint/rotate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"os": "windows"}'
```

### A.14 Health Check

```bash
curl -s http://127.0.0.1:9867/health \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
# Response: {"status":"ok","tabs":1}
```

### A.15 Known Documentation Errors

| Source | Error | Correct Value |
|--------|-------|---------------|
| pinchtab.com (`/docs/tabs-api/`) | Lock path: `POST /tabs/{id}/lock` | Both work, but flat `POST /tab/lock` is canonical |
| pinchtab.com | Lock field: `ttl` | `timeoutSec` |
| pinchtab.com | `GET /tabs` returns flat array | Returns `{"tabs": [...]}` |
| pinchtab.com | Snapshot params: `interactive=true`, `compact=true` | `filter=interactive`, `format=compact` |
| `api-reference.json` | Lock field: `ttl` | `timeoutSec` |
| Root `pinchtab-api.md` | Resource-based paths throughout | Flat paths are canonical |
