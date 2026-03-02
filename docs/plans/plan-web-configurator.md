# Web Configurator — Detailed Implementation Plan

## Status

- **State:** Draft v2 (comprehensive revision of the original plan)
- **Issue:** [#7](https://github.com/shantanugoel/oxydra/issues/7)
- **Scope:** `types` + `runner` crates, plus documentation updates
- **Prerequisite:** Issue #17 logging infrastructure (already complete: `collect_logs_snapshot`, `RunnerLogEntry`, control-socket log operations)

---

## Review of Original Plan — Identified Issues

This section documents problems found in the original plan that this revision addresses.

### 1. Lifecycle Mismatch: Web Server vs. Daemon

**Problem:** The original plan assumes the web server runs inside the per-user daemon, but the daemon only exists *after* `runner start`. The web configurator's primary value is *before* and *between* daemon runs — initial setup, editing config when the system is stopped, starting the first user.

**Fix:** The web configurator runs as its own lifecycle, independent of the per-user daemon. It is started via `runner web` and communicates with running daemons via the existing control socket protocol. When no daemon is running, config read/write and status reporting still work. Lifecycle commands (start/stop/restart) are proxied to the daemon via the control socket.

### 2. First-Run / Onboarding Completely Missing

**Problem:** The original plan has no onboarding flow. A brand new user opening the web UI for the first time sees empty/default configs and has no guidance on what to do.

**Fix:** Add a first-run wizard that detects missing/default configurations and guides the user through: creating `runner.toml` if it doesn't exist, registering their first user, configuring at least one provider, and starting the daemon.

### 3. PUT Semantics Are Dangerous

**Problem:** The original issue's API includes `PUT` (full replace) endpoints for config files. A full replace can accidentally wipe fields the user didn't include in the request, destroy TOML comments, and create broken configs.

**Fix:** Remove `PUT` entirely. Use only `PATCH` with RFC 7396 JSON Merge Patch semantics. Null values in the patch explicitly remove keys. All other keys are preserved. TOML comment preservation is handled by `toml_edit`.

### 4. Provenance Engine is Over-Engineered for V1

**Problem:** The original plan specifies a full field-level provenance engine (showing which layer each value came from). This requires walking figment's merge chain at a per-field level, which figment doesn't natively support for post-extract introspection. Building this from scratch is complex and fragile.

**Fix:** For V1, show a simpler but still very useful model:
- Show the **source file value** (what's in the TOML file).
- Show the **effective runtime value** (what the daemon is actually using, if running).
- Show **known override indicators** (env var names that could override, CLI flags).
- Defer full per-field provenance tracking to V2.

### 5. Multi-File Transaction is YAGNI for V1

**Problem:** The original plan specifies a multi-file rollback transaction system. In V1, the only multi-file operation is adding a new user (create user TOML + add entry to `runner.toml`). Full transactional semantics are overkill.

**Fix:** Use ordered writes with backup: back up all files, apply changes, and if any step fails, restore from backups. No formal transaction protocol needed.

### 6. Operation Tracking Adds Unnecessary Complexity

**Problem:** The original plan introduces an operation queue with `202 Accepted` + `operation_id` + polling for restart/reload. This adds significant state management for operations that complete in seconds.

**Fix:** For V1, restart/stop/reload are synchronous HTTP requests with reasonable timeouts. The web UI shows a spinner. If the operation takes too long, the client gets a timeout error and can retry. Operation tracking can be added in V2 if needed.

### 7. SSE Log Streaming Has State Management Issues

**Problem:** The original plan proposes SSE log streaming by polling `collect_logs_snapshot()`. This creates hash-based dedup state on the server, doesn't handle reconnections well, and has no backpressure mechanism.

**Fix:** Use a simpler polling model from the frontend. The browser polls `GET /api/v1/logs/{user_id}?since=<last_timestamp>` every 2 seconds. This is stateless on the server, handles reconnections trivially, and the `since` parameter provides natural dedup. SSE can be added in V2 if needed.

### 8. Alpine.js Single-File Approach Won't Scale

**Problem:** The original issue proposes inline CSS+JS in a single HTML file. With dashboard, config editor (3 config types), control panel, logs viewer, and onboarding wizard, this will become unmaintainable at 2000+ lines in a single file.

**Fix:** Use a small set of embedded files: `index.html` (shell + router), a few JS modules (one per page), and a CSS file. Still no build pipeline — plain ES modules that browsers natively support. Embedded via `rust-embed` as before. Total: 5-8 files instead of 1 monolith.

### 9. Schema Endpoint Couples Types Crate to schemars

**Problem:** Adding `schemars` + `JsonSchema` derives to all config structs in the `types` crate adds a dependency to the foundation layer, increasing compile time for every crate in the workspace.

**Fix:** Don't use `schemars`. The frontend forms are hand-crafted (they need to be anyway for good UX — auto-generated forms from JSON Schema are always a poor user experience). The backend returns typed JSON with field metadata (type, default value, description, constraints) as a purpose-built struct, not a generic JSON Schema.

### 10. Missing: Config File Doesn't Exist Yet

**Problem:** The original plan assumes all config files already exist. On a fresh install, `.oxydra/runner.toml` and `.oxydra/agent.toml` may not exist.

**Fix:** Handle missing files gracefully:
- If a config file doesn't exist, return defaults with a `"file_exists": false` flag.
- PATCH on a non-existent file creates it with the patched defaults.
- The onboarding wizard handles initial file creation.

### 11. Reload Semantics Are Undefined

**Problem:** The original plan includes a `reload` endpoint but doesn't define what can actually be reloaded without restart. The current codebase has no hot-reload mechanism.

**Fix:** Remove `reload` from V1. All config changes require restart. The UI clearly indicates this with a "restart required" banner after any config change is saved.

### 12. Security: CSRF via DNS Rebinding

**Problem:** Even on `127.0.0.1`, browsers can be tricked via DNS rebinding attacks to make requests from malicious pages. The original plan mentions origin checks but doesn't detail the implementation.

**Fix:** Enforce `Host` header validation on every request (must match the configured bind address). Add `Content-Type` enforcement on all mutation endpoints. These two together block DNS rebinding attacks without requiring a CSRF token.

---

## Goals (Revised)

1. Give operators a local web dashboard for status, config editing, control, and logs.
2. Provide a guided first-run experience for new installations.
3. Keep config TOML files as the single source of truth (no DB, no shadow state).
4. Work when the daemon is stopped (config editing, status checking).
5. Work when the daemon is running (live status, logs, lifecycle control).
6. Preserve TOML comments and formatting during edits.
7. Maintain Oxydra security posture and strict crate boundaries.
8. Keep the total footprint minimal (embedded UI, no Node/npm/build toolchain).

## Non-Goals (V1)

1. Full field-level provenance engine (deferred to V2).
2. Hot-reload of config without restart.
3. SSE/WebSocket live streaming (polling is fine for V1).
4. Multi-user simultaneous dashboard (the web server is per-installation, not per-user).
5. Auto-generated forms from JSON Schema.
6. Dark mode (deferred to V2).

---

## Architecture

### Runtime Topology

The web configurator is a separate subcommand that runs its own HTTP server. It operates independently of the per-user daemon lifecycle.

```
runner web [--bind 127.0.0.1:9400] [--config .oxydra/runner.toml]
  ├─ serves embedded SPA on /
  ├─ serves REST API on /api/v1/*
  ├─ reads/writes config files directly
  └─ proxies lifecycle commands to per-user daemon control sockets
```

```
┌────────────────────────────────────────────────────────────────┐
│                        Host Machine                             │
│                                                                 │
│  ┌──────────────────────┐         ┌─────────────────────┐      │
│  │  runner web           │         │  runner daemon       │      │
│  │  (Axum HTTP server)   │  unix   │  (per-user, may or  │      │
│  │                       │  sock   │   may not be running)│      │
│  │  Config TOML R/W  ────┼─────────┤                     │      │
│  │  Status/Control   ────┼─────────┤  control socket      │      │
│  │  Logs             ────┼─────────┤  log files           │      │
│  └──────────┬────────────┘         └─────────────────────┘      │
│             │ HTTP                                               │
│       ┌─────▼──────┐     ┌────────────┐                         │
│       │  Browser   │     │ Config     │                         │
│       │  (SPA)     │     │ TOML files │                         │
│       └────────────┘     └────────────┘                         │
└─────────────────────────────────────────────────────────────────┘
```

### Why a Separate Subcommand (Not Embedded in Daemon)

1. **Works when daemon is stopped** — The most common use case (initial setup, config editing, checking status after a crash) requires the web server to work independently.
2. **No lifecycle coupling** — Restarting a user daemon doesn't kill the web UI.
3. **Multi-user visibility** — The web server can show status for all registered users and start/stop any of them.
4. **Clean separation** — The daemon's control loop doesn't need to manage HTTP server lifecycle.

The web server CAN also be started alongside the daemon (future `runner start --web` flag) but the default is a separate process for V1.

### Module Layout

```
crates/runner/src/
  web/
    mod.rs                # Axum router, server startup, CLI wiring
    state.rs              # Shared web server state (config paths, user registry)
    middleware.rs          # Auth, host validation, content-type enforcement
    response.rs           # Standard API response envelope, error codes
    status.rs             # GET /status, GET /status/{user_id}
    config_read.rs        # GET config endpoints (runner, user, agent)
    config_write.rs       # PATCH config endpoints, backup, validation
    control.rs            # POST start/stop/restart endpoints
    logs.rs               # GET logs endpoints
    onboarding.rs         # GET /onboarding/status, first-run detection
    masking.rs            # Secret masking for API responses
    static_files.rs       # rust-embed file serving

crates/runner/
  static/
    index.html            # SPA shell (nav, router, Alpine.js init)
    js/
      app.js              # Alpine.js app initialization, router, shared state
      dashboard.js        # Dashboard page component
      config-editor.js    # Config editor page component (reused for all 3 types)
      control.js          # Control panel page component
      logs.js             # Log viewer page component
      onboarding.js       # First-run wizard component
    css/
      style.css           # All styles (light theme, responsive)
```

---

## Configuration

### New `[web]` Section in `runner.toml`

```toml
[web]
enabled = true                          # default: true
bind = "127.0.0.1:9400"                # default loopback only
# Auth mode: "disabled" (local-only default) or "token"
auth_mode = "disabled"
# When auth_mode = "token", read bearer token from this env var
auth_token_env = "OXYDRA_WEB_TOKEN"
# Fallback: inline token (discouraged, prefer env var)
# auth_token = ""
```

### New Type in `types/src/runner.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebConfig {
    #[serde(default = "default_web_enabled")]
    pub enabled: bool,
    #[serde(default = "default_web_bind")]
    pub bind: String,
    #[serde(default = "default_web_auth_mode")]
    pub auth_mode: WebAuthMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WebAuthMode {
    #[default]
    Disabled,
    Token,
}
```

---

## API Design

### Response Envelope

All API responses use a consistent envelope:

```json
// Success
{
  "data": { ... },
  "meta": { "request_id": "..." }
}

// Error
{
  "error": {
    "code": "config_validation_failed",
    "message": "Human-readable description",
    "details": { ... }
  },
  "meta": { "request_id": "..." }
}
```

Error codes are stable strings (not numbers). The frontend matches on `error.code` for localized handling.

### Error Code Taxonomy

| Code | HTTP Status | Meaning |
|------|-------------|---------|
| `not_found` | 404 | Resource (user, config file) not found |
| `config_validation_failed` | 422 | Config changes failed validation |
| `config_write_failed` | 500 | File system write error |
| `daemon_not_running` | 409 | Lifecycle command sent but no daemon running |
| `daemon_already_running` | 409 | Start requested but daemon is already running |
| `daemon_command_failed` | 502 | Control socket command failed |
| `daemon_command_timeout` | 504 | Control socket command timed out |
| `unauthorized` | 401 | Missing or invalid auth token |
| `invalid_content_type` | 415 | Wrong Content-Type on mutation endpoint |
| `invalid_request` | 400 | Malformed request body |

### Endpoints

#### Onboarding

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/onboarding/status` | Returns `{ "needs_setup": bool, "missing": [...] }` — detects if `runner.toml` exists, if any users are registered, if a provider has an API key configured |

#### System Status

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/meta` | Server version, config path, workspace root |
| `GET` | `/api/v1/status` | Aggregated status: all registered users and their daemon state (running/stopped, health if running) |
| `GET` | `/api/v1/status/{user_id}` | Detailed status for a specific user. Proxies to control socket if daemon is running; returns `daemon_not_running` otherwise |

#### Runner Config

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/config/runner` | Returns `RunnerGlobalConfig` as JSON, with secrets masked. Includes `file_exists: bool` and `file_path: string` |
| `PATCH` | `/api/v1/config/runner` | RFC 7396 JSON Merge Patch. Creates file if missing. Returns `{ "changed_fields": [...], "backup_path": "...", "restart_required": bool }` |
| `POST` | `/api/v1/config/runner/validate` | Dry-run validation of a proposed patch. Returns validation result without writing |

#### User Config

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/config/users` | List all registered users with summary info |
| `GET` | `/api/v1/config/users/{user_id}` | Returns user's `RunnerUserConfig` as JSON with secrets masked |
| `PATCH` | `/api/v1/config/users/{user_id}` | RFC 7396 patch on user config |
| `POST` | `/api/v1/config/users/{user_id}/validate` | Dry-run validation |
| `POST` | `/api/v1/config/users` | Register a new user: creates user TOML file + adds entry to `runner.toml` |
| `DELETE` | `/api/v1/config/users/{user_id}` | Unregister user (removes from `runner.toml`, optionally deletes config file) |

#### Agent Config

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/config/agent` | Returns workspace `AgentConfig` as JSON with secrets masked. Includes `file_exists: bool` |
| `GET` | `/api/v1/config/agent/effective` | If daemon is running: returns the effective merged config (what the runtime actually loaded). If not running: returns the result of loading through figment with all layers |
| `PATCH` | `/api/v1/config/agent` | RFC 7396 patch on workspace `.oxydra/agent.toml` |
| `POST` | `/api/v1/config/agent/validate` | Dry-run validation |

#### Lifecycle Control

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/v1/control/{user_id}/start` | Start user daemon. Accepts optional `{ "insecure": bool }`. Blocks until gateway endpoint is available (with timeout) |
| `POST` | `/api/v1/control/{user_id}/stop` | Stop user daemon. Blocks until shutdown completes (with timeout) |
| `POST` | `/api/v1/control/{user_id}/restart` | Stop then start. Blocks until new daemon is ready |

#### Logs

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/logs/{user_id}` | Query params: `role` (runtime/sidecar/all), `stream` (stdout/stderr/both), `tail` (default 200), `since` (RFC 3339 or duration), `format` (json). Proxies to daemon control socket |

### Endpoint Notes

- All mutation endpoints (`PATCH`, `POST`, `DELETE`) require `Content-Type: application/json`.
- All mutation endpoints validate the `Host` header matches the configured bind address.
- All endpoints return `request_id` in the response meta for debugging.
- When auth is enabled, all `/api/v1/*` endpoints require `Authorization: Bearer <token>`.
- Static file serving (`/`, `/js/*`, `/css/*`) does not require auth.

---

## Config Read/Write Implementation

### Reading Config Files

```rust
// Pseudocode for GET /api/v1/config/agent
fn get_agent_config(state: &WebState) -> ApiResponse {
    let path = state.workspace_dir.join("agent.toml");
    if !path.exists() {
        return ok(AgentConfigResponse {
            config: AgentConfig::default(),
            file_exists: false,
            file_path: path.display().to_string(),
        });
    }
    let raw = fs::read_to_string(&path)?;
    let config: AgentConfig = toml::from_str(&raw)?;
    // Mask secrets before returning
    let masked = mask_secrets(config);
    ok(AgentConfigResponse {
        config: masked,
        file_exists: true,
        file_path: path.display().to_string(),
    })
}
```

### Writing Config Files (PATCH)

The write path uses `toml_edit` (already in the runner's dependencies) to preserve comments and formatting:

```
1. Read current TOML file (or empty document if file doesn't exist)
2. Parse into toml_edit::DocumentMut (preserves comments/formatting)
3. Parse incoming JSON merge patch
4. Apply patch fields to the document:
   - For each key in the patch:
     - If value is null: remove the key from the document
     - If value is a secret sentinel ("__UNCHANGED__"): skip (keep current value)
     - Otherwise: set/update the key
5. Deserialize the modified document into the typed config struct
6. Run validate() on the typed struct
7. If validation fails: return error, do not write
8. If validation passes:
   a. Create backup: copy original to <file>.bak.<timestamp>
   b. Write modified document to <file>.tmp
   c. fsync the temp file
   d. Atomic rename <file>.tmp -> <file>
   e. Prune old backups (keep last 10)
   f. Return success with changed fields list and backup path
```

### Backup Policy

- Each write creates a backup at `<file>.bak.<unix_timestamp_millis>`.
- Keep the most recent 10 backups per file.
- Failed writes do not create backups (the temp file is cleaned up).
- Backup pruning happens after successful writes only.

### Secret Masking

Fields masked in API responses:
- `providers.registry.*.api_key` → `"********"` (8 asterisks)
- `memory.auth_token` → `"********"`
- `web.auth_token` → `"********"`
- `channels.telegram.bot_token_env` → shown as-is (it's an env var name, not a secret)
- `credential_refs` values → `"********"`

On PATCH:
- If a masked field's value is `"__UNCHANGED__"`: preserve the current value from the file.
- If a masked field's value is `""` (empty string): clear it.
- If a masked field has any other value: set the new value.
- The frontend sends `"__UNCHANGED__"` for masked fields the user didn't edit.

---

## Security

### Default Posture

- Bind `127.0.0.1:9400` (loopback only).
- No auth required in local-only mode (with a warning banner in the UI).
- Secrets masked in all API responses.
- All mutation endpoints enforce `Content-Type: application/json`.
- `Host` header validated on every request.

### Auth Mode

When `auth_mode = "token"`:
- All `/api/v1/*` requests must include `Authorization: Bearer <token>`.
- Token is resolved: first from the env var named in `auth_token_env`, then from inline `auth_token`.
- Token comparison uses constant-time equality (`ring::constant_time::verify_slices_are_equal` or manual impl).
- Static file serving (SPA assets) does not require auth (they contain no secrets).
- Failed auth returns `401` with `"unauthorized"` error code.

### DNS Rebinding Protection

- Every request validates the `Host` header against the configured `bind` address.
- Requests where `Host` doesn't match are rejected with `403`.
- This blocks DNS rebinding attacks where a malicious page tries to reach `127.0.0.1:9400` via a rebinding domain.

### CORS Policy

- CORS is disabled by default (no `Access-Control-Allow-Origin` header).
- The embedded SPA is served from the same origin, so CORS is not needed.
- If someone wants to access the API from a different origin (e.g., a custom frontend), they can use a reverse proxy.

### Rate Limiting

- Not implemented in V1 (loopback-only default makes this low risk).
- Add in V2 if remote access becomes common.

---

## Frontend Design

### Technology Stack

- **Alpine.js** (latest, ~15KB gzipped): Reactive data binding, component lifecycle.
- **ES Modules**: Each page is a separate `.js` file loaded via native `import`. No bundler.
- **Hash-based routing**: `#/dashboard`, `#/config/agent`, etc. Simple `window.onhashchange` handler.
- **`fetch()` API**: All API calls use native fetch with a thin wrapper for auth headers and error handling.

### Page Structure

```
index.html
├── Navigation sidebar (always visible)
│   ├── Dashboard
│   ├── Agent Config
│   ├── Runner Config
│   ├── Users
│   ├── Logs
│   └── Status indicator (connected/auth status)
├── Main content area (swapped by router)
└── Toast notification area
```

### Pages

#### 1. Dashboard (`#/`)

- **System overview cards**: For each registered user, show name, daemon status (running/stopped), sandbox tier, available tools, degraded reasons.
- **Quick actions**: Start/Stop/Restart buttons per user.
- **Active alerts**: Any warnings from running daemons.
- **Meta info**: Oxydra version, config file locations.
- **First-run banner**: If `needs_setup` is true, show a prominent "Get Started" card linking to the onboarding wizard.

#### 2. Onboarding Wizard (`#/setup`)

Shown when the system detects an incomplete setup. Steps:

1. **Welcome**: Brief explanation of what Oxydra does and what the configurator helps with.
2. **Runner Config**: Create or verify `runner.toml`. Set workspace root.
3. **Register User**: Create the first user. Set the user config path.
4. **Provider Setup**: Configure at least one LLM provider. Test the API key (optional ping).
5. **Review & Start**: Show a summary of all configurations. Button to start the daemon.

Each step validates before proceeding. Back navigation is supported. The wizard creates files progressively (not all at the end).

#### 3. Agent Config Editor (`#/config/agent`)

- **Sections** (collapsible): Selection, Runtime, Memory, Providers, Reliability, Tools, Scheduler, Agents.
- **Each field shows**: Current value, default value (as placeholder), description tooltip.
- **Secret fields**: Masked with a reveal toggle. Edit sends `__UNCHANGED__` unless modified.
- **Validation**: Client-side basic validation (required fields, numeric ranges, weight sums). Server-side full validation on save.
- **Save flow**: Edit → "Save" button → Calls `/validate` first → Shows diff preview → Calls `PATCH` → Shows "Restart required" banner if daemon is running.
- **Effective value panel** (sidebar): If the daemon is running, shows the effective runtime value for each section. Highlights fields that differ from the file value (indicating env/CLI overrides).

#### 4. Runner Config Editor (`#/config/runner`)

- Same UX pattern as agent config editor.
- Sections: General (workspace root, default tier), Guest Images, Web Config.
- User list is shown here but individual user configs are edited in the Users page.

#### 5. Users Page (`#/config/users`)

- **User list**: Table with user ID, config path, daemon status, quick actions.
- **Add User**: Form to register a new user (user ID, config path).
- **User Detail** (expandable or separate sub-route `#/config/users/{id}`): Full user config editor (mounts, resources, credentials, behavior overrides, channels).
- **Delete User**: Confirmation dialog. Option to also delete the config file.

#### 6. Logs Page (`#/logs`)

- **User selector**: Dropdown to pick which user's logs to view.
- **Filters**: Role (runtime/sidecar/all), Stream (stdout/stderr/both), Tail count.
- **Log display**: Monospace, syntax-highlighted by log level. Auto-scroll to bottom.
- **Auto-refresh**: Toggle to poll every 2 seconds for new entries.
- **Clear/Refresh**: Manual buttons.

### Frontend Embedding

Files in `crates/runner/static/` are embedded into the binary using `rust-embed`:

```rust
#[derive(rust_embed::Embed)]
#[folder = "static/"]
struct StaticAssets;
```

The SPA shell (`index.html`) is served at `/`. All `static/js/*` and `static/css/*` files are served with appropriate `Content-Type` headers and `Cache-Control: no-cache` (for development; production can use content-hash based cache busting in V2).

Alpine.js is vendored (downloaded and placed in `static/js/vendor/alpine.min.js`) so there's zero CDN dependency.

---

## Implementation Phases

### Phase 1: Types + Web Server Bootstrap

**Goal:** `runner web` starts an HTTP server that serves a blank SPA and health endpoint.

**Steps:**

1. **Add `WebConfig` to `RunnerGlobalConfig`** (in `types/src/runner.rs`):
   - Add `WebConfig` struct with `enabled`, `bind`, `auth_mode`, `auth_token_env`, `auth_token` fields.
   - Add `#[serde(default)]` field `pub web: WebConfig` to `RunnerGlobalConfig`.
   - Add validation: parse bind address, validate auth_mode consistency (if token mode, at least one token source must be set).
   - Update default config version if needed.
   - Run config migration if the field is added to existing runner.toml files (no-op: serde defaults handle it).

2. **Add `web` CLI subcommand** (in `runner/src/main.rs`):
   - Add `Web { #[arg(long)] bind: Option<String> }` variant to `CliCommand`.
   - Wire up to a new `handle_web()` function.

3. **Create `runner/src/web/mod.rs`** — Axum router:
   - Health check at `GET /api/v1/meta` returning version info.
   - Static file serving for embedded assets.
   - Server startup with graceful shutdown on Ctrl+C.

4. **Create `runner/src/web/state.rs`** — `WebState`:
   - Holds: `RunnerGlobalConfig`, config file path, resolved workspace root.
   - Constructed from the same `Runner::from_global_config_path()` used by the CLI.

5. **Create `runner/src/web/static_files.rs`** — `rust-embed` integration:
   - Serve embedded static files with MIME type detection.
   - Fallback to `index.html` for SPA routing.

6. **Create minimal `static/index.html`** — SPA shell:
   - Load Alpine.js from vendored file.
   - Show "Oxydra Web Configurator" heading and navigation skeleton.

7. **Add new dependencies to `runner/Cargo.toml`**:
   - `rust-embed` (latest) with `interpolate-folder-path` feature.
   - `mime_guess` (latest) for content-type detection.
   - `tower-http` is NOT needed (auth/cors handled manually in middleware).

**Verification gate:**
- `runner web` starts and `http://127.0.0.1:9400` shows the SPA shell.
- `GET /api/v1/meta` returns JSON with version info.
- Existing runner CLI commands (`start`, `stop`, `status`, etc.) are unaffected.
- All tests pass, no clippy warnings.

---

### Phase 2: Config Read Endpoints + Dashboard

**Goal:** The web UI can display current configuration and system status.

**Steps:**

1. **Create `runner/src/web/masking.rs`** — Secret masking:
   - `mask_agent_config(config: &AgentConfig) -> serde_json::Value` — serializes to JSON, then walks the tree and masks known secret paths.
   - `mask_runner_config(config: &RunnerGlobalConfig) -> serde_json::Value` — same for runner config.
   - `mask_user_config(config: &RunnerUserConfig) -> serde_json::Value` — same for user config.
   - Uses a const list of secret paths: `["providers.registry.*.api_key", "memory.auth_token", "web.auth_token", "credential_refs.*"]`.

2. **Create `runner/src/web/response.rs`** — API envelope:
   - `ApiResponse<T>` struct with `data`, `meta` fields.
   - `ApiError` struct with `code`, `message`, `details` fields.
   - `impl IntoResponse` for both.
   - Request ID generation (short random hex string per request).

3. **Create `runner/src/web/config_read.rs`** — Read endpoints:
   - `GET /api/v1/config/runner` — Load and return masked runner config.
   - `GET /api/v1/config/agent` — Load and return masked agent config from workspace dir.
   - `GET /api/v1/config/users` — List registered users with summary.
   - `GET /api/v1/config/users/{user_id}` — Load and return masked user config.
   - `GET /api/v1/config/agent/effective` — Attempt to load the full figment-merged agent config (from all layers). If daemon is running, also include runtime values.
   - Each response includes `file_exists`, `file_path`, and `has_daemon_running` flags.

4. **Create `runner/src/web/status.rs`** — Status endpoints:
   - `GET /api/v1/status` — For each registered user, check if control socket exists and is responsive. Return aggregated status.
   - `GET /api/v1/status/{user_id}` — If daemon running, proxy health check to control socket. If not, return `daemon_not_running`.

5. **Create `runner/src/web/onboarding.rs`** — First-run detection:
   - `GET /api/v1/onboarding/status` — Check: does runner.toml exist? Are any users registered? Does agent.toml exist? Is at least one provider configured with a key (or key env var that resolves)?
   - Return `{ "needs_setup": bool, "checks": { "runner_config": bool, "has_users": bool, "agent_config": bool, "has_provider": bool } }`.

6. **Build Dashboard page** (`static/js/dashboard.js`):
   - Fetch `/api/v1/status` and `/api/v1/onboarding/status` on load.
   - Render user status cards, meta info, onboarding banner.

7. **Build Config viewer pages** (`static/js/config-editor.js`):
   - Reusable component that renders a config object as a structured form (read-only in this phase).
   - Sections are collapsible. Secrets are masked with reveal toggle.
   - Used for Agent, Runner, and User config pages.

**Verification gate:**
- Dashboard shows registered users and their daemon status.
- Config pages display current configuration with masked secrets.
- Missing config files show defaults with "file not found" indicator.
- All tests pass, no clippy warnings.

---

### Phase 3: Config Write + Validation

**Goal:** The web UI can edit and save configuration files.

**Steps:**

1. **Create `runner/src/web/config_write.rs`** — Write infrastructure:
   - `backup_file(path: &Path) -> Result<PathBuf>` — Creates timestamped backup.
   - `prune_backups(path: &Path, keep: usize)` — Removes old backups.
   - `atomic_write(path: &Path, content: &str) -> Result<()>` — Write to temp + fsync + rename.
   - `apply_json_merge_patch(document: &mut DocumentMut, patch: &serde_json::Value, secret_sentinel: &str)` — Walks the patch and applies changes to the `toml_edit` document, skipping sentinel values.
   - `compute_changed_fields(before: &serde_json::Value, after: &serde_json::Value) -> Vec<String>` — Returns dot-separated paths of fields that changed.

2. **Implement PATCH endpoints**:
   - `PATCH /api/v1/config/runner` — Read current file (or empty doc), apply merge patch, validate typed struct, atomic write with backup.
   - `PATCH /api/v1/config/agent` — Same for agent.toml.
   - `PATCH /api/v1/config/users/{user_id}` — Same for user config.
   - All PATCH responses include: `changed_fields`, `backup_path`, `restart_required` (true if daemon is running).

3. **Implement validation endpoints**:
   - `POST /api/v1/config/runner/validate` — Apply patch in memory, validate, return result without writing.
   - `POST /api/v1/config/agent/validate` — Same.
   - `POST /api/v1/config/users/{user_id}/validate` — Same.

4. **Implement user management endpoints**:
   - `POST /api/v1/config/users` — Body: `{ "user_id": "...", "config_path": "..." }`. Creates user TOML with defaults, adds user registration to runner.toml. Both writes use backup+atomic.
   - `DELETE /api/v1/config/users/{user_id}` — Query param: `delete_config_file=true/false`. Removes from runner.toml, optionally deletes config file.

5. **Create `runner/src/web/middleware.rs`** — Security middleware:
   - `host_validation_layer()` — Rejects requests where `Host` header doesn't match bind address.
   - `content_type_enforcement()` — Rejects mutation requests without `application/json` content type.
   - `auth_layer()` — When auth is enabled, validates `Authorization: Bearer <token>` with constant-time comparison.

6. **Update config editor frontend to be editable**:
   - Fields become editable inputs (text, number, boolean toggle, select).
   - "Save" button appears when there are unsaved changes.
   - Save flow: validate → show diff → confirm → PATCH → toast notification.
   - "Restart required" banner shown after saving if daemon is running.
   - Secret fields: show `__UNCHANGED__` placeholder. Only send actual value if user explicitly edits.

7. **Build Onboarding Wizard** (`static/js/onboarding.js`):
   - Step-by-step guided setup using the same PATCH endpoints.
   - Provider setup step tries to resolve the API key env var and shows a green checkmark.

**Verification gate:**
- Config changes are saved to disk with backups created.
- TOML comments are preserved after edits (tested with a file containing comments).
- Invalid configs are rejected with clear error messages.
- New user registration creates both files correctly.
- Secret sentinel behavior works correctly (unchanged secrets are preserved).
- Host header validation rejects mismatched hosts.
- Auth middleware blocks unauthenticated requests when enabled.
- All tests pass, no clippy warnings.

---

### Phase 4: Lifecycle Control + Logs

**Goal:** The web UI can start/stop/restart daemons and view logs.

**Steps:**

1. **Create `runner/src/web/control.rs`** — Lifecycle endpoints:
   - `POST /api/v1/control/{user_id}/start` — Uses `Runner::start_user()` to launch daemon. Waits for gateway endpoint (with 30s timeout). Returns startup info.
   - `POST /api/v1/control/{user_id}/stop` — Sends `ShutdownUser` via control socket. Waits for socket removal (with 10s timeout).
   - `POST /api/v1/control/{user_id}/restart` — Stop (if running) then start.
   - **Important:** The web server itself does NOT become a daemon. The `Runner::start_user()` method spawns the guest processes. The web server needs to hold onto the `RunnerStartup` handle to keep the processes alive. This requires adding an `Arc<Mutex<HashMap<String, RunnerStartup>>>` to `WebState` that tracks active user daemons started by the web server.
   - **Alternative:** If the web server only starts daemons in background (like `runner start` does with its daemon loop), we need the web server to fork/spawn the daemon as a separate background process. This is cleaner — use `std::process::Command` to spawn `runner start --user <id>` as a detached child process. The web server then communicates with it via the control socket like the CLI does.

   **Decision: Use the spawn approach.** The web server spawns `runner start --user <id>` as a child process (detached). Lifecycle management goes through the control socket. This avoids the web server needing to hold daemon state, and means `runner web` can be stopped/restarted without killing running daemons.

2. **Create `runner/src/web/logs.rs`** — Log endpoints:
   - `GET /api/v1/logs/{user_id}` — Proxy to daemon control socket using `RunnerControl::Logs(request)`. If daemon not running, try to read log files directly from the workspace logs directory.
   - Query params: `role`, `stream`, `tail`, `since`, `format`.
   - Response uses the existing `RunnerControlLogsResponse` format.

3. **Build Control Panel page** (`static/js/control.js`):
   - Start/Stop/Restart buttons per user.
   - Status polling (every 5 seconds when page is visible).
   - Button states reflect daemon status (disable "Start" when running, etc.).
   - Spinner during operations, toast on success/failure.

4. **Build Logs page** (`static/js/logs.js`):
   - User selector dropdown.
   - Log output area with monospace font, colored by log level.
   - "Auto-refresh" toggle that polls every 2 seconds.
   - Filter controls for role, stream, tail count.
   - "Copy to clipboard" button.

5. **Wire daemon lifecycle into web state**:
   - Track spawned daemon child process PIDs in `WebState` for cleanup on web server shutdown.
   - On web server shutdown (Ctrl+C), log a warning but do NOT kill running daemons (they're independent processes).

**Verification gate:**
- Start/Stop/Restart from the web UI works.
- Logs are displayed with correct formatting.
- Auto-refresh shows new log entries.
- Starting an already-running daemon returns appropriate error.
- Stopping a stopped daemon returns appropriate error.
- Web server shutdown doesn't kill running daemons.
- All tests pass, no clippy warnings.

---

### Phase 5: Polish + Testing + Documentation

**Goal:** Production-ready quality, comprehensive tests, documentation.

**Steps:**

1. **Frontend polish**:
   - Responsive layout (works on tablet-width screens).
   - Loading states (skeleton screens or spinners) for all API calls.
   - Error boundary: unexpected errors show a friendly "something went wrong" message.
   - Keyboard navigation: Tab through fields, Enter to save.
   - Toast notifications: success (green), error (red), warning (yellow), with auto-dismiss.

2. **Audit logging**:
   - Each mutation endpoint logs: timestamp, endpoint, changed fields, backup path, result (success/failure).
   - Logs go to the tracing infrastructure (not a separate audit log file in V1).

3. **Test suite** — Unit tests:
   - `masking.rs`: Secret paths are masked, non-secret paths are not.
   - `config_write.rs`: Backup creation, pruning, atomic write, merge patch application, sentinel handling, comment preservation.
   - `middleware.rs`: Host validation, content-type enforcement, auth token verification (constant-time).
   - `onboarding.rs`: First-run detection logic for various combinations of existing/missing files.

4. **Test suite** — Integration tests:
   - Start web server, make API calls, verify responses.
   - Config round-trip: read config → patch → read again → verify changes persisted.
   - Config validation: send invalid patches, verify 422 responses.
   - Auth: verify protected endpoints reject unauthenticated requests.
   - Status: verify correct status when daemon is running vs. stopped.
   - Lifecycle: start daemon via web API, verify it's running, stop it, verify it's stopped.

5. **Test suite** — CLI compatibility:
   - Verify `runner start/stop/status/logs` CLI commands still work alongside `runner web`.
   - Verify config edits via web UI are visible to CLI commands.

6. **Documentation updates**:
   - Update guidebook Chapter 2 (Configuration System) with web configurator info.
   - Update guidebook Chapter 8 (Runner) with `runner web` subcommand.
   - Add operator guide: how to access the web UI, how to enable auth for remote access, how to use with a reverse proxy.
   - Update `README.md` with web configurator feature.
   - Update Chapter 15 with web configurator phase status.

7. **Verify all quality gates**:
   - `cargo fmt --check` passes.
   - `cargo clippy` with no warnings.
   - `cargo deny check` passes.
   - `cargo nextest run` passes all tests.
   - No `#[allow(clippy::...)]` directives added.

**Verification gate:**
- All tests pass.
- Documentation is complete and accurate.
- Web UI is usable for a complete new-installation workflow.
- Web UI is usable for day-to-day config editing and status monitoring.
- Existing CLI workflows are unaffected.

---

## Dependency Policy

### Rule

All new dependencies use the latest stable release at implementation time. Add with `cargo add <crate>` (never manually pin to stale versions). Run `cargo update` after additions. Validate with `cargo tree -d` and `cargo deny check`.

### New Dependencies

| Crate | Purpose | Added To |
|-------|---------|----------|
| `rust-embed` (latest) | Embed static files into binary | `runner` |
| `mime_guess` (latest) | Content-type detection for embedded files | `runner` |

### Dependencies NOT Added (from original plan, with reasoning)

| Crate | Reason Not Added |
|-------|-----------------|
| `schemars` | Not needed — forms are hand-crafted, not auto-generated from JSON Schema |
| `tower-http` (cors) | Not needed — CORS disabled, middleware is manual |
| `json-merge-patch` | Not needed — merge patch logic is ~50 lines of code, not worth a dependency |
| `fd-lock` | Not needed for V1 — single writer (web server process), file locking adds complexity without benefit |

### Dependencies Already Available

| Crate | Already In | Used For |
|-------|------------|----------|
| `axum` | `runner` | HTTP server, routing |
| `toml_edit` | `runner` | Comment-preserving TOML writes |
| `toml` | `runner` | TOML parsing (reading) |
| `serde_json` | `runner` | JSON serialization |
| `serde` | `runner` | Serialization framework |
| `tokio` | `runner` | Async runtime |
| `tracing` | `runner` | Logging |

---

## Risks and Mitigations

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | **TOML comment preservation is lossy** — `toml_edit` may reformat some constructs. | Medium | Low | Test with a representative config file. Accept minor reformatting. Document that inline comments on the same line as a value may shift. |
| 2 | **Config drift** — User edits TOML by hand while web UI is open; web UI overwrites their changes. | Medium | Medium | On PATCH, read the file fresh (don't cache). Add an `If-Match` / ETag header based on file mtime+size. If the file changed since the client loaded it, return `409 Conflict` with the new content. |
| 3 | **Daemon spawn failures** — `runner start` fails but the web UI doesn't show why. | Medium | Medium | Capture the child process stdout/stderr and return it in the error response. |
| 4 | **Future config fields not in the UI** — New config fields added to Rust structs won't automatically appear in the frontend. | Certain | Low | This is expected. The structured form approach means new fields require frontend work. Document this as a maintenance task. An "Advanced / Raw TOML" tab (V2) can serve as escape hatch. |
| 5 | **Alpine.js size grows** — Complex UI logic may make JS files large. | Low | Low | Each page is a separate file. Individual files should stay under 500 lines. If they grow, split into sub-components. |
| 6 | **Concurrent web server instances** — User accidentally runs `runner web` twice. | Low | Medium | On startup, check if the bind address is already in use. Print a clear error message. |
| 7 | **Config file permissions** — Web server may not have write permission to config files. | Low | High | Check write permissions on startup and warn. Return clear error on write failure. |

---

## Keeping in Sync with Future Changes

### When a new config field is added to Rust structs:

1. **Backend automatically handles it** — The read endpoints serialize the full typed struct to JSON. New fields appear in the API response immediately (with their default values).
2. **Frontend needs manual update** — Add the new field to the appropriate form in the config editor. This is intentional: each field deserves a human-readable label, description, and appropriate input type.
3. **Masking list may need update** — If the new field contains secrets, add it to the masking path list.

### When a new CLI command is added:

- The web configurator may or may not need to expose it. Evaluate case by case.
- The `/api/v1/status` endpoint automatically picks up health changes via the control socket protocol.

### When the runner protocol changes:

- The web server uses `send_control_to_daemon()` for lifecycle operations and `RunnerControl::Logs()` for logs — same functions as the CLI. Protocol changes automatically propagate.

### Testing strategy for sync:

- A simple compile-time assertion test: serialize `AgentConfig::default()` and `RunnerGlobalConfig::default()` to JSON. If any field is added or removed, the test output changes. This doesn't enforce frontend sync, but it alerts developers that the API shape changed.

---

## Acceptance Checklist

- [ ] `runner web` subcommand starts an HTTP server on the configured bind address.
- [ ] Dashboard shows registered users and their daemon status.
- [ ] Onboarding wizard guides first-time setup (runner config → user → provider → start).
- [ ] Config editors display current configuration with masked secrets.
- [ ] Config editors allow editing and saving with validation.
- [ ] Saved configs preserve TOML comments and formatting.
- [ ] Backups are created before each write.
- [ ] Start/Stop/Restart buttons work from the web UI.
- [ ] Logs are viewable from the web UI with filtering.
- [ ] Auth middleware blocks unauthenticated requests when token auth is enabled.
- [ ] Host header validation blocks cross-origin requests.
- [ ] Secret masking prevents API key leakage.
- [ ] Existing CLI commands (`runner start/stop/status/logs/restart`) remain fully functional.
- [ ] All new Rust code has unit tests.
- [ ] Integration tests cover the full config read → edit → save → verify cycle.
- [ ] Guidebook documentation is updated.
- [ ] No clippy warnings, all tests pass, `cargo deny` clean.
- [ ] All new dependencies use latest stable versions.
