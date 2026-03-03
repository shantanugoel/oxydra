# Browser Use — Skill-Based Approach

> **Status:** Proposed
> **Author:** AI Agent
> **Created:** 2026-03-03
> **Related:** Chapter 14 (Skill System), Chapter 4 (Tool System), Chapter 8 (Runner/Guests)

## 1. Overview

This plan adds browser automation to Oxydra using a **skill-based approach**:
the LLM drives Pinchtab's REST API directly via `curl` through the existing
`shell_exec` tool, guided by a skill document injected into the system prompt.

This also introduces a general-purpose **skill system** — a way to teach agents
domain-specific workflows via markdown documents. Browser automation is the
first built-in skill; the system supports arbitrary user-authored skills.

### Why a skill instead of a dedicated tool

A dedicated `browser_use` tool would wrap Pinchtab's REST API in Rust (~2000+
lines), registering a JSON schema with 26+ action types and per-action handlers.
The skill approach avoids this entirely:

| Concern | Dedicated tool | Skill (shell + curl) |
|---|---|---|
| **API drift** | Every Pinchtab API change requires code changes, tests, recompilation | Update a markdown file |
| **Provider schema issues** | Each provider has JSON schema quirks (e.g., Gemini requires `items` on all arrays) | No schema — uses existing `shell_exec` |
| **Maintenance** | ~2000 lines of Rust + mock server + tests | ~200 lines of markdown |
| **Batching** | Custom batch execution engine | Pinchtab's native `POST /actions` endpoint, or shell `&&` chaining |
| **Flexibility** | New capabilities need new enum variants + handlers | LLM uses any endpoint immediately |

The LLM is already excellent at composing `curl` commands and parsing JSON — this
is a core strength of modern models. The shell runs inside the sandboxed
container, providing the same isolation boundary as a dedicated tool.

---

## 2. Skill System Architecture

The skill system is general-purpose — not specific to browser automation. Any
domain knowledge that helps the LLM use existing tools more effectively can be
packaged as a skill.

### 2.1 Skill vs Tool — decision criteria

| Use a **tool** when… | Use a **skill** when… |
|---|---|
| The operation needs Rust-side logic (crypto, binary protocol, sandbox enforcement) | The operation is a standard REST/CLI workflow the LLM can drive via shell |
| The interface is stable and internal to Oxydra | The interface is external, evolving, or third-party |
| Structured input validation is critical | Target service's error responses provide sufficient feedback |
| The operation must be atomic | Sequential commands with error checking are sufficient |

### 2.2 Skill File Format

Each skill lives in its own folder with a `SKILL.md` file containing YAML
frontmatter and markdown body:

```
config/skills/BrowserAutomation/
├── SKILL.md                    # skill body (frontmatter + content)
└── references/                 # optional reference files
    └── pinchtab-api.md
```

`SKILL.md` format:

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

You can control a headless Chrome browser via Pinchtab's HTTP API...
```

**Frontmatter fields:**

| Field | Required | Default | Description |
|---|---|---|---|
| `name` | Yes | — | Unique identifier (kebab-case). Used for deduplication across directories |
| `description` | Yes | — | One-line summary for diagnostics and future UI |
| `activation` | No | `auto` | `auto` — inject when all conditions met; `manual` — only on explicit request; `always` — always inject |
| `requires` | No | `[]` | Tool names that must be registered for this skill to activate |
| `env` | No | `[]` | Environment variables that must be set. Values are available for `{{VAR}}` template substitution in the skill body. **Only use for non-sensitive values** (URLs, paths, feature flags). Secrets must be referenced as shell env vars (`$VAR`) in the skill body — the shell expands them at runtime, keeping actual values out of the system prompt. |
| `priority` | No | `100` | Ordering when multiple skills are active (lower = earlier in prompt) |

### 2.3 Skill Storage

Skills are organized as **folders**, each containing a `SKILL.md` file and
optionally a `references/` subdirectory with supplementary files:

```
config/skills/                              # built-in skills (ship with Oxydra)
├── BrowserAutomation/
│   ├── SKILL.md                            # skill body (frontmatter + markdown)
│   └── references/
│       └── pinchtab-api.md                 # lazy-loaded reference docs
├── SimpleSkill/
│   └── SKILL.md                            # simple skill (no references)
<user_config_dir>/skills/                   # user-level skills (shared across workspaces)
├── MyCustomSkill/
│   └── SKILL.md
<workspace_root>/.oxydra/skills/            # workspace-level skills (project-specific)
├── ProjectWorkflow/
│   └── SKILL.md
```

Bare `.md` files directly in a `skills/` directory are also supported for
backward compatibility (simple skills without reference files).

Skills are discovered with workspace-level taking highest precedence: a skill
with the same `name` at a higher-precedence level replaces the lower one
entirely (no merging). This lets users override or disable built-in skills.

The `config/` directory at the repo root holds all shipped configuration
including built-in skills.

When skill folders are copied to the workspace at session setup, they are placed
at `/shared/.oxydra/skills/<SkillFolder>/` preserving the folder structure. This
avoids naming collisions between different skills' reference files.

### 2.4 Skill Loading and Activation

At session start:

1. **Discover** — `SkillLoader` scans all three directories, deduplicates by
   `name` (workspace > user > system).
2. **Evaluate** — For `auto` skills: check that all `requires` tools are
   registered in the `ToolRegistry` **and available** (i.e.,
   `ToolAvailability` reports the corresponding capability as ready — not just
   that the tool name exists in the registry), and all `env` vars are set.
   Mark as active if conditions are met.

   > **Why readiness, not registration:** In the current runtime, `shell_exec`
   > is always registered even when the shell sidecar is unavailable or
   > disabled (it returns an error on invocation). Checking only registration
   > would activate the browser skill in contexts where the shell cannot
   > actually execute commands. The `SkillLoader` therefore receives both the
   > `ToolRegistry` and `ToolAvailability` and gates on readiness.
3. **Render** — Active skills have `{{VAR}}` placeholders replaced with env
   var values.
4. **Inject** — Rendered skill content is appended to the system prompt.

Activation is evaluated once at session start. Dynamic re-evaluation is not
needed initially — Pinchtab availability is known at startup.

### 2.5 Skill Injection and Token Management

Active skill content is appended to the system prompt alongside existing
sections (scheduler, memory, specialists):

```rust
let skills_note = render_active_skills(&active_skills, &env_vars);
format!("...{shell_note}{scheduler_note}{memory_note}{skills_note}{specialists_note}")
```

#### Token impact

The system prompt is sent with every API call. Skill content adds to its size.
Current system prompt sections for context:

| Section | Approx. tokens |
|---|---|
| Base prompt (identity, file system) | ~300 |
| Memory section (when enabled) | ~800 |
| Scheduler section (when enabled) | ~150 |
| **Browser skill (proposed)** | **~1200** |

The browser skill at ~1200 tokens is comparable to the memory section. This
content benefits from **prompt caching** — providers like Anthropic cache
stable system prompt prefixes, so the skill content is processed once and served
from cache on subsequent turns.

#### Token cap

Skills are capped at **3000 tokens** (~12KB). The `SkillLoader` estimates token
count (chars ÷ 4) and warns/rejects skills exceeding the cap. This prevents
prompt bloat from overly verbose skills.

#### Lazy loading via references

For skills that need extensive reference material (full API docs, parameter
tables, etc.), the skill body contains a concise summary with a pointer to a
reference file:

```markdown
For the full API reference with all parameters, run:
cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md
```

The reference file is placed in the workspace at session setup time — skill
folders (including their `references/` subdirectory) are copied from
`config/skills/` to `/shared/.oxydra/skills/`. The LLM reads reference files
on demand via `shell_exec` or `file_read` only when needed — a multi-page API
reference doesn't inflate every turn's system prompt.

This pattern — concise skill in prompt, detailed reference on disk — keeps the
prompt lean while giving the LLM access to full documentation when it needs it.

### 2.6 Scope Boundaries

The initial skill system intentionally omits:

- **LLM-authored skills.** Skills are human-authored, loaded at session start.
  Chapter 14's vision of LLM skill creation is a future extension.
- **Dynamic activation mid-session.** Skills are evaluated once at startup.
- **Inter-skill dependencies.** Each skill is self-contained.
- **Skill-specific tools.** Skills teach the LLM to use existing tools. New
  tools are added independently through the tool system.

---

## 3. Browser Automation Skill

### 3.1 Pinchtab's Official Skill

Pinchtab ships an [official skill](https://github.com/pinchtab/pinchtab/tree/main/skill/pinchtab)
(`SKILL.md` + `references/api.md` + `references/env.md` + `references/profiles.md`).

**Assessment:**

| Aspect | Verdict | Notes |
|---|---|---|
| `SKILL.md` (~1460 tokens) | Good starting point | Covers workflow, snapshot format, tips. But includes setup instructions (starting Pinchtab, env vars) that don't apply to our managed container environment |
| `references/api.md` (~2325 tokens) | Excellent reference | Comprehensive curl examples for all endpoints. Too large for system prompt injection but perfect as a lazy-loaded reference file |
| `references/env.md` (~580 tokens) | Not needed in prompt | Environment config is handled by the runner, not the LLM |
| `references/profiles.md` (~680 tokens) | Not needed initially | Profile management is orchestrator/dashboard mode; our container runs bridge mode |
| `TRUST.md` (~670 tokens) | Not needed | Security info for humans, not agents |

**Decision:** Use Pinchtab's official `api.md` as the lazy-loaded reference
file. Write a custom condensed skill body (~1200 tokens) tailored to our bridge
mode deployment — strip setup instructions, add Oxydra-specific context (file
paths, `send_media` integration, human assistance).

### 3.2 File I/O Integration

Browser automation frequently involves file operations:

- **Screenshots/PDFs** — Pinchtab saves files to disk. The LLM needs to know
  they're at `/shared/` paths and can be sent to users via `send_media`.
- **Downloads** — Pinchtab's `GET /download?url=...&output=file&path=...`
  saves downloaded files. The LLM may need to read them with `file_read`.
- **Uploads** — Pinchtab's `POST /upload` takes local file paths. The LLM may
  need to prepare files in `/shared/` first using `file_write`, then reference
  them in the upload.
- **Extracted text** — The LLM might save page text to `/shared/` for later
  processing.

The skill prompt explicitly documents these integration points so the LLM knows
how to combine browser operations with file tools:

```markdown
### File Integration
- Save screenshots: `curl "{{PINCHTAB_URL}}/tabs/$TAB/screenshot" -o /shared/screenshot.png`
- Save PDFs: `curl "{{PINCHTAB_URL}}/tabs/$TAB/pdf?output=file&path=/shared/page.pdf"`
- Download files: `curl "{{PINCHTAB_URL}}/download?url=...&output=file&path=/shared/report.csv"`
- Upload files: First write to /shared/, then `curl -X POST "{{PINCHTAB_URL}}/upload" ...`
- Send to user: After saving to /shared/, use `send_media` to deliver the file
```

### 3.3 Skill Body (System Prompt Injection)

This is the condensed content injected into every system prompt when browser
automation is active (~1200 tokens):

```markdown
## Browser Automation (Pinchtab)

You can control a headless Chrome browser via Pinchtab at `{{PINCHTAB_URL}}`.
All curl commands require the auth header: `-H "Authorization: Bearer $BRIDGE_TOKEN"`
Chrome starts lazily on the first request.

### Core Loop

1. **Navigate** → creates a tab, returns `tabId`
2. **Snapshot** → accessibility tree with clickable refs (e.g., `e5`)
3. **Act** → click/type/fill/press using refs
4. **Snapshot again** → use `diff=true` to see only changes (~90% fewer tokens)
5. Repeat 3-4 until done

```bash
# Navigate (creates new tab)
TAB=$(curl -s -X POST {{PINCHTAB_URL}}/navigate \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com"}' | jq -r '.tabId')

# Snapshot (interactive elements with refs)
curl -s "{{PINCHTAB_URL}}/tabs/$TAB/snapshot?filter=interactive&maxTokens=2000" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"

# Click a ref
curl -s -X POST "{{PINCHTAB_URL}}/tabs/$TAB/action" \
  -H "Authorization: Bearer $BRIDGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"kind":"click","ref":"e5"}'

# Diff snapshot (only changes)
curl -s "{{PINCHTAB_URL}}/tabs/$TAB/snapshot?filter=interactive&diff=true" \
  -H "Authorization: Bearer $BRIDGE_TOKEN"
```

### Key Endpoints

| Endpoint | Method | Purpose |
|---|---|---|
| `/navigate` | POST | Navigate URL → `{tabId, url, title}`. Creates new tab if no `tabId` in body |
| `/tabs/{id}/navigate` | POST | Navigate existing tab |
| `/tabs` | GET | List all tabs |
| `/tab` | POST | `{"action":"new"}` or `{"action":"close","tabId":"..."}` |
| `/tabs/{id}/snapshot` | GET | Accessibility tree. Params: `filter=interactive`, `diff=true`, `maxTokens=2000`, `format=compact` |
| `/tabs/{id}/text` | GET | Readable text. Params: `mode=raw`, `maxChars=N` |
| `/tabs/{id}/action` | POST | `{"kind":"click\|type\|fill\|press\|hover\|scroll\|select\|focus", "ref":"e5", ...}` |
| `/tabs/{id}/actions` | POST | Batch: `{"actions":[...], "stopOnError":true}` |
| `/tabs/{id}/screenshot` | GET | Binary PNG → save with `curl -o /shared/file.png` |
| `/tabs/{id}/pdf` | GET/POST | `?output=file&path=/shared/page.pdf` |
| `/tabs/{id}/evaluate` | POST | Run JS: `{"expression":"document.title"}` |
| `/tabs/{id}/cookies` | GET/POST | Get/set cookies |
| `/download` | GET | Download file: `?url=...&output=file&path=/shared/file.ext` |
| `/upload` | POST | Upload file to input: `{"selector":"input[type=file]","paths":["/shared/file.jpg"]}` |

### Best Practices

- Always use `maxTokens=2000` on snapshots (full trees can exceed 10K tokens)
- Use `filter=interactive` to see only clickable/input elements
- Use `diff=true` after actions for ~90% token savings
- Use `/text` for reading content (~800 tokens/page)
- Use `format=compact` for most token-efficient snapshots
- Batch interactions with `POST /tabs/{id}/actions` for fewer round-trips
- Save files to `/shared/` and use `send_media` to deliver to user
- Wait 2-3 seconds after navigation before snapshot for complex pages: `sleep 3`
- Store `$TAB` and reuse it — tab IDs are stable

### If Blocked

If you encounter CAPTCHAs, 2FA, or login walls, call `request_human_assistance`
with a clear description of the blocker.

For the full API reference (all params, PDF options, upload/download, stealth):
`cat /shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
```

### 3.4 Reference File (Lazy-loaded)

Pinchtab's official `references/api.md` (~2325 tokens) is placed at
`/shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md` during session setup. The LLM
reads it on demand when it needs detailed parameter information (PDF options,
stealth settings, advanced cookie management, etc.).

This file is copied from `config/skills/BrowserAutomation/references/pinchtab-api.md`
in the Oxydra distribution.

### 3.5 Shell Policy Requirements

The browser skill's curl-based workflow requires commands and shell features
that are **not** in the default shell allowlist. The following must be allowed
for browser automation to function:

| Requirement | Default status | Needed by |
|---|---|---|
| `curl` | ❌ Not in default allowlist | All Pinchtab API calls |
| `jq` | ❌ Not in default allowlist | Parsing JSON responses, extracting `tabId` |
| `sleep` | ❌ Not in default allowlist | Waiting for page loads before snapshot |
| Shell operators (`&&`, `$()`, `\|`) | ❌ Blocked by default | Command chaining, variable capture |

**Resolution:** When the runner detects `BROWSER_ENABLED=true`, it
automatically applies a shell config overlay that adds `curl`, `jq`, `sleep`
to the allowlist and enables `allow_operators`. This is done in the runner's
container provisioning, not via a separate user config step — browser
automation should work out of the box when enabled.

The runner constructs a `ShellConfig` overlay:

```rust
// When browser is enabled, extend shell policy for browser skill commands
if browser_enabled {
    shell_config.allow.get_or_insert_with(Vec::new)
        .extend(["curl", "jq", "sleep"].map(str::to_owned));
    shell_config.allow_operators = Some(true);
}
```

Users who have custom shell configs retain full control — the runner only
adds to the allowlist, never replaces user-specified settings.

---

## 4. Infrastructure (Pinchtab in Shell-VM)

The container infrastructure is independent of whether we use a tool or skill
approach. This section covers getting Pinchtab running inside the shell-vm
container.

### 4.1 Container Changes

Pinchtab runs alongside `shell-daemon` in the same container. The shell-vm
Docker image already installs Chromium. Adding the ~12 MB Pinchtab Go binary
keeps the topology simple.

**Entrypoint script** (`docker/shell-vm-entrypoint.sh`):

```sh
#!/bin/sh
set -e

if [ "${BROWSER_ENABLED}" = "true" ]; then
    # Clean up Chrome locks from previous crashes
    PROFILES_DIR="${BRIDGE_STATE_DIR:-/shared/.pinchtab}/profiles"
    if [ -d "${PROFILES_DIR}" ]; then
        find "${PROFILES_DIR}" -name "SingletonLock" -delete 2>/dev/null || true
        find "${PROFILES_DIR}" -name ".com.google.Chrome.SingletonLock" -delete 2>/dev/null || true
    fi

    /usr/local/bin/pinchtab &
    PINCHTAB_PID=$!

    # Wait for Pinchtab to become healthy (up to 30s).
    # The runner also polls /health externally, but this avoids a race where
    # shell-daemon starts accepting commands before Pinchtab is ready.
    HEALTH_URL="http://${BRIDGE_BIND:-127.0.0.1}:${BRIDGE_PORT:-9867}/health"
    RETRIES=0
    MAX_RETRIES=30
    while [ $RETRIES -lt $MAX_RETRIES ]; do
        if curl -sf -o /dev/null "$HEALTH_URL" 2>/dev/null; then
            break
        fi
        # Check if Pinchtab process is still alive
        if ! kill -0 $PINCHTAB_PID 2>/dev/null; then
            echo "WARNING: Pinchtab process exited unexpectedly" >&2
            break
        fi
        RETRIES=$((RETRIES + 1))
        sleep 1
    done
    if [ $RETRIES -eq $MAX_RETRIES ]; then
        echo "WARNING: Pinchtab health check timed out after ${MAX_RETRIES}s" >&2
    fi
fi

exec /usr/local/bin/shell-daemon "$@"
```

**Dockerfile additions** (both `Dockerfile` and `Dockerfile.prebuilt`):

```dockerfile
ARG PINCHTAB_VERSION=0.7.6

# Install required tools (curl, jq for browser skill, chromium for Pinchtab)
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl jq chromium \
    && rm -rf /var/lib/apt/lists/*

# Download Pinchtab binary for the target architecture
RUN ARCH=$(dpkg --print-architecture) && \
    curl -fSL "https://github.com/pinchtab/pinchtab/releases/download/v${PINCHTAB_VERSION}/pinchtab-linux-${ARCH}" \
      -o /usr/local/bin/pinchtab && \
    chmod +x /usr/local/bin/pinchtab

COPY docker/shell-vm-entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
```

### 4.2 Environment Variables

The runner configures these env vars for the shell-vm container:

| Variable | Source | Purpose |
|---|---|---|
| `BRIDGE_PORT` | Runner-assigned (sequential from 9867) | Pinchtab HTTP port |
| `BRIDGE_BIND` | Fixed: `127.0.0.1` | Bind to loopback only |
| `BRIDGE_TOKEN` | Runner-generated default, user-overridable | Auth token |
| `BRIDGE_HEADLESS` | Config | Default headless mode |
| `BRIDGE_STEALTH` | Config | Stealth level (`light` or `full`) |
| `BRIDGE_STATE_DIR` | Fixed: `/shared/.pinchtab` | Profile/state storage |
| `CHROME_BINARY` | Fixed: `/usr/bin/chromium` | Chrome path |
| `CHROME_FLAGS` | Fixed: `--no-sandbox` | Required for containerized Chrome |
| `BROWSER_ENABLED` | Config | Whether to start Pinchtab |

The same `BRIDGE_TOKEN` value and `PINCHTAB_URL` (`http://127.0.0.1:<port>`) are
also set in the oxydra-vm environment so the skill system can template them.

### 4.3 Auth Token Strategy

The runner generates a random `BRIDGE_TOKEN` at container provisioning time. It
is:

1. **Set as env var in shell-vm** — Pinchtab enforces it on all requests.
2. **Set as env var in shell-vm's shell environment** — available as
   `$BRIDGE_TOKEN` for the LLM's curl commands.
3. **Referenced in the skill prompt** — the skill instructs the LLM to include
   `-H "Authorization: Bearer $BRIDGE_TOKEN"` in curl commands. The shell
   expands `$BRIDGE_TOKEN` at runtime; the actual token value never appears in
   the system prompt.

The token can be overridden by the user in their config (runner config or web
configurator). If not overridden, the auto-generated default is used.

Since both containers share host networking and Pinchtab binds to `127.0.0.1`,
the token is defense-in-depth — only local processes can reach Pinchtab
regardless.

### 4.4 Port Allocation

Multiple users on the same host need distinct Pinchtab ports. The runner uses a
**probe-and-reserve** strategy starting from port 9867:

1. Try binding a TCP listener to the candidate port.
2. If the port is in use, increment and retry (up to 100 attempts).
3. Once a free port is found, record it in the user's runtime state.

This avoids collisions across restarts, concurrent users, and other local
processes. The assigned port is included in the bootstrap envelope so the
oxydra-vm knows where to connect.

### 4.5 Profile Support (Follow-up — not in MVP)

> **Note:** Profile support is deferred until stateless browser automation is
> stable. The infrastructure below describes the intended design for a
> follow-up iteration.

Pre-baked browser profiles let agents skip login flows. Users create profiles
locally (with headed Chrome), and they're mounted into the container.

**Storage:** `<workspace>/.pinchtab/` — already mounted as `/shared/.pinchtab`
inside the container via the existing workspace bind-mount.

**Creation workflow:**
```bash
# Human runs Pinchtab locally with visible Chrome
BRIDGE_HEADLESS=false BRIDGE_STATE_DIR=/path/to/workspace/.pinchtab pinchtab
# Log into sites, close Chrome. Profile persists.
```

**Agent usage** (bridge mode doesn't have profile management endpoints; the
profile is loaded at Pinchtab startup via `BRIDGE_PROFILE` env var):
```bash
# Cookies/storage carry over from the pre-baked profile
curl -X POST http://localhost:9867/navigate \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://mail.google.com"}'
# → Already logged in if profile has valid cookies
```

### 4.6 Health Check

The `/health` endpoint is the **sole readiness signal** for browser
availability. The runner polls `GET /health` after starting the container:

1. Poll every 1s, up to 30s timeout.
2. On success: set `browser_available = true` in `StartupStatusReport`.
3. On timeout or Pinchtab crash: set `browser_available = false`, log a
   warning, but continue — the shell remains fully functional. The browser
   skill will not activate (its `env` condition `PINCHTAB_URL` won't be set).
4. If Pinchtab crashes mid-session, curl commands will return connection
   errors. The LLM will see these and can report the issue. A future
   enhancement could monitor the Pinchtab process and attempt restart.

---

## 5. Component Changes

### 5.1 New: `types/src/skill.rs`

```rust
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub activation: SkillActivation,
    pub requires: Vec<String>,
    pub env_vars: Vec<String>,
    pub priority: i32,
}

pub enum SkillActivation {
    Auto,
    Manual,
    Always,
}

pub struct Skill {
    pub metadata: SkillMetadata,
    pub content: String,         // Markdown body (post-frontmatter)
    pub source_path: PathBuf,
}
```

### 5.2 New: Skill loader in `runner` crate (`runner/src/skills.rs`)

The skill loader lives in the `runner` crate because it is fundamentally about
prompt construction and session configuration — the same domain as
`build_system_prompt()` and config loading. It depends on `ToolAvailability`
(from the `tools` crate) but does not need the `ToolRegistry` itself.

```rust
pub struct SkillLoader { /* directory paths */ }

impl SkillLoader {
    /// Scan directories, parse frontmatter, deduplicate by name.
    pub fn discover(&self) -> Vec<Skill>;

    /// Filter to active skills based on tool availability and env vars.
    /// Uses `ToolAvailability` (not `ToolRegistry`) to check readiness.
    pub fn evaluate(
        skills: &[Skill],
        availability: &ToolAvailability,
        env: &HashMap<String, String>,
    ) -> Vec<&Skill>;

    /// Render a skill's content with env var substitution.
    pub fn render(skill: &Skill, env: &HashMap<String, String>) -> String;
}
```

Dependencies: `gray_matter` (0.3.2) for frontmatter parsing, `serde` + `serde_yaml`
(already transitive deps).

### 5.3 Modified: `runner/src/bootstrap.rs`

The `build_system_prompt()` function gains a `{skills_note}` placeholder:

```rust
fn build_system_prompt(
    paths: &ConfigSearchPaths,
    bootstrap: Option<&RunnerBootstrapEnvelope>,
    scheduler_enabled: bool,
    memory_enabled: bool,
    agents: &BTreeMap<String, AgentDefinition>,
    active_skills: &[RenderedSkill],  // NEW
) -> Option<String> {
    // ... existing notes ...
    let skills_note = format_skills_note(active_skills);
    format!("...{shell_note}{scheduler_note}{memory_note}{skills_note}{specialists_note}")
}
```

### 5.4 Modified: `types/src/runner.rs`

Add `BrowserToolConfig` (already partially exists from Phase 1 infrastructure):

```rust
pub struct BrowserToolConfig {
    pub pinchtab_base_url: String,
    pub bridge_token: Option<String>,
}
```

Add `browser_config: Option<BrowserToolConfig>` to `RunnerBootstrapEnvelope`.

### 5.5 New: `config/skills/BrowserAutomation/SKILL.md`

The built-in browser skill (Section 3.3 content).

### 5.6 New: `config/skills/BrowserAutomation/references/pinchtab-api.md`

Adapted from Pinchtab's official `references/api.md`. Placed at
`/shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md` during session setup.

### 5.7 Docker changes

Both `Dockerfile` and `Dockerfile.prebuilt`:
- Download Pinchtab binary from GitHub releases (multi-arch aware)
- Install `curl`, `jq` (required by browser skill and entrypoint health check)
- Add `shell-vm-entrypoint.sh`
- Change entrypoint to the launcher script

### 5.8 Runner changes

- Port allocation for Pinchtab (probe-and-reserve)
- `BRIDGE_TOKEN` generation (random hex, stored in config or generated per-session)
- Environment variable forwarding to shell-vm
- `PINCHTAB_URL` env var for oxydra-vm
- Health check polling (with timeout and graceful degradation)
- Shell policy overlay: auto-add `curl`, `jq`, `sleep` and enable operators
  when `BROWSER_ENABLED=true`
- Copy skill reference files to workspace during session setup
- `browser_config` in bootstrap envelope

---

## 6. Implementation Phases

### Phase A: Skill System Foundation ✅

**Goal:** A minimal, working skill system that can load markdown skills and
inject them into the system prompt.

**Status:** Complete

**Scope:**
1. ✅ Define `Skill`, `SkillActivation`, `SkillMetadata`, `RenderedSkill` types in `types/src/skill.rs`
2. ✅ Add `gray_matter = "0.3"` dependency (to `runner` crate, with `yaml` feature)
3. ✅ Implement skill loader in `runner/src/skills.rs`:
   - ✅ Parse YAML frontmatter from markdown files
   - ✅ Scan skill directories (system/config → user → workspace)
   - ✅ Support both folder-based skills (`SkillFolder/SKILL.md`) and bare
     `.md` files
   - ✅ Deduplicate by name (workspace wins)
   - ✅ Evaluate activation conditions using `ToolAvailability` (readiness, not
     just registration) and env var presence
   - ✅ Template substitution for `{{ENV_VAR}}` placeholders (non-sensitive
     values only — see Section 2.2)
   - ✅ Token estimation and 3000-token cap enforcement
4. ✅ Modify `build_system_prompt()` to accept and inject active skills
5. ✅ Wire up: load skills at session start, pass to prompt builder
6. ✅ Unit tests (36 total): frontmatter parsing, directory precedence, deduplication,
   activation evaluation (including readiness checks), template rendering,
   token cap, prompt injection, folder-based discovery
7. ✅ **Test: skill activation does NOT occur when shell is registered but
   unavailable** (e.g., `ToolAvailability.shell` is `Unavailable`)

**Verification gate:** ✅ Skills discovered from config directory, activated based
on conditions (including tool readiness), rendered with env vars, injected into
system prompt. Token cap enforced.

### Phase B: Pinchtab Infrastructure ✅

**Goal:** Pinchtab runs inside the shell-vm container and is reachable from
oxydra-vm.

**Status:** Complete

**Scope:**
1. ✅ Add Pinchtab binary to shell-vm Docker image (both Dockerfiles)
   - Updated `docker/Dockerfile` and `docker/Dockerfile.prebuilt` to use
     `shell-vm-entrypoint.sh` instead of direct `shell-daemon` entrypoint
   - Pinchtab binary downloaded from GitHub releases at build time
     (multi-arch: `dpkg --print-architecture` resolves to `amd64` or `arm64`)
   - `docker/Dockerfile` updated to install `curl` and `jq` (required by
     entrypoint health check and browser skill)
   - `PINCHTAB_VERSION` build arg allows pinning to a specific release
2. ✅ Create `docker/shell-vm-entrypoint.sh` (with health-check readiness loop)
3. ✅ Add `BrowserToolConfig` to types, `browser_config` to bootstrap envelope
4. ✅ Add env var forwarding in runner (`BRIDGE_*`, `CHROME_*`, `BROWSER_ENABLED`)
5. ✅ Port allocation logic (probe-and-reserve from 9867)
6. ✅ `BRIDGE_TOKEN` generation and forwarding
7. ✅ Set `PINCHTAB_URL` env var in oxydra-vm environment
8. ✅ Health check polling (`GET /health`) with 30s timeout and graceful
   degradation — entrypoint script polls health internally; if Pinchtab
   health check fails, `PINCHTAB_URL` is not set so browser skill won't
   activate, but shell remains fully functional
9. ✅ Shell policy overlay: auto-add `curl`, `jq`, `sleep` to allowlist and
   enable `allow_operators` when `BROWSER_ENABLED=true`
10. ✅ Copy skill folders (including reference files) to `/shared/.oxydra/skills/` during setup
    - Created `config/skills/BrowserAutomation/references/pinchtab-api.md`
      adapted from official Pinchtab API docs
    - Skill folders copied preserving structure:
      `config/skills/BrowserAutomation/references/` →
      `/shared/.oxydra/skills/BrowserAutomation/references/`
11. ✅ **Test: Pinchtab health failure keeps shell usable and browser unavailable**
    - `browser_skill_inactive_when_shell_unavailable`
    - `browser_skill_inactive_when_pinchtab_url_missing`
    - `startup_with_browser_disabled_has_no_browser_config`
    - `startup_process_tier_never_provisions_browser`
12. ✅ **Test: browser skill activation toggles correctly with browser
    availability and env vars**
    - `browser_skill_activates_when_shell_ready_and_pinchtab_url_set`
    - `browser_skill_inactive_when_pinchtab_url_missing`
    - `browser_skill_inactive_when_shell_unavailable`
    - `browser_skill_renders_pinchtab_url`
    - `startup_with_browser_enabled_populates_browser_env_in_shell_vm`

**Tests added (22 total):**
- Port allocation: `find_available_port_returns_some_port_in_range`
- Token generation: `generate_bridge_token_produces_hex_string`,
  `generate_bridge_token_produces_unique_values`
- Browser env: `build_browser_env_returns_expected_vars`
- Shell overlay: `apply_browser_shell_overlay_adds_required_commands`,
  `apply_browser_shell_overlay_does_not_duplicate_existing_commands`,
  `apply_browser_shell_overlay_preserves_existing_deny_and_replace_defaults`
- Skill ref copy: `copy_skill_reference_files_copies_to_target`,
  `copy_skill_reference_files_is_noop_when_source_missing`,
  `copy_skill_reference_files_skips_folders_without_references`
- Bootstrap envelope: `browser_config_in_bootstrap_envelope_round_trips`
- Integration: `startup_with_browser_enabled_populates_browser_env_in_shell_vm`,
  `startup_with_browser_disabled_has_no_browser_config`,
  `startup_process_tier_never_provisions_browser`
- Skill activation: `browser_skill_activates_when_shell_ready_and_pinchtab_url_set`,
  `browser_skill_inactive_when_pinchtab_url_missing`,
  `browser_skill_inactive_when_shell_unavailable`,
  `browser_skill_renders_pinchtab_url`

**Verification gate:** ✅ Browser infrastructure is provisioned when
`browser_enabled=true`: port allocated, token generated, env vars forwarded to
both VMs, shell policy overlay applied, skill folders (including references)
copied. When browser is disabled or in Process tier, no browser infrastructure
is provisioned. Skill system correctly gates browser skill activation on tool
readiness and `PINCHTAB_URL` presence.

### Phase C: Browser Automation Skill ✅

**Goal:** The LLM can drive the browser via shell commands guided by the skill.

**Status:** Complete

**Scope:**
1. ✅ Write `config/skills/BrowserAutomation/SKILL.md` (Section 3.3)
   - Full skill body (~1036 estimated tokens) with Core Loop, Key Endpoints
     table, Best Practices, File Integration, and If Blocked sections
   - `{{PINCHTAB_URL}}` template placeholders for env substitution
   - `$BRIDGE_TOKEN` referenced as shell env var (never in prompt)
   - Lazy-loaded reference pointer to
     `/shared/.oxydra/skills/BrowserAutomation/references/pinchtab-api.md`
2. ✅ Adapt Pinchtab's `references/api.md` as
   `config/skills/BrowserAutomation/references/pinchtab-api.md`
   (already done in Phase B)
3. ✅ Verify skill auto-activates when shell is available + `PINCHTAB_URL` is set
   - `actual_browser_skill_discovers_and_activates`
   - `actual_browser_skill_does_not_activate_without_shell`
4. ✅ Integration test: skill appears in system prompt under correct conditions
   - `actual_browser_skill_appears_in_formatted_prompt`
   - `actual_browser_skill_absent_from_prompt_without_pinchtab_url`
5. ✅ **Test: end-to-end command path works under the shell policy that
   includes browser allowlist additions**
   - `browser_shell_overlay_allows_browser_skill_commands` — verifies curl,
     curl|jq pipe, sleep, jq standalone, curl -o, && chaining all pass;
     wget is still blocked
   - `default_policy_rejects_browser_commands` — verifies curl, jq, sleep
     are rejected without the overlay
6. Manual end-to-end test: agent navigates, reads, clicks via shell+curl
   (requires live container — deferred to deployment validation)
7. **Manual test: navigate → snapshot → click/type → diff snapshot →
   screenshot to `/shared` → `send_media`**
   (requires live container — deferred to deployment validation)

**Tests added (9 total):**
- Skill parsing: `actual_browser_skill_parses_with_correct_metadata`
- Content validation: `actual_browser_skill_content_within_token_cap_and_has_key_sections`
- Activation: `actual_browser_skill_discovers_and_activates`,
  `actual_browser_skill_does_not_activate_without_shell`
- Rendering: `actual_browser_skill_renders_with_pinchtab_url_substituted`
- Prompt integration: `actual_browser_skill_appears_in_formatted_prompt`,
  `actual_browser_skill_absent_from_prompt_without_pinchtab_url`
- Shell policy: `browser_shell_overlay_allows_browser_skill_commands`,
  `default_policy_rejects_browser_commands`

**Additional fixes:**
- **Embedded skills:** Built-in skills are now compiled into the oxydra-vm
  binary via `rust-embed`. The skill loader uses embedded skills as the
  lowest-priority tier — filesystem skills (system → user → workspace)
  override embedded ones by name. This eliminates the need for Dockerfile
  `COPY` or tarball bundling for skills. Works across all tiers (Container,
  Process, MicroVM).
- **Reference extraction:** At startup, oxydra-vm extracts embedded reference
  files (e.g. `pinchtab-api.md`) to `<shared>/.oxydra/skills/` so the LLM
  can `cat` them from the shell. The previous `copy_skill_reference_files()`
  in the runner is removed — oxydra-vm is now self-contained for skill data.
- **User overrides:** Users can override built-in skills by placing their own
  `SKILL.md` at `~/.config/oxydra/skills/<SkillName>/SKILL.md` (user-level)
  or `<workspace>/.oxydra/skills/<SkillName>/SKILL.md` (workspace-level).
- **Logging:** Skill discovery and activation now log at `info` level:
  discovered skills count/names, activation results, and specific reasons
  for non-activation (missing tools, missing env vars). This makes it easy
  to diagnose why a skill is not injected.

**Additional tests (5 new):**
- `embedded_skills_include_browser_automation`
- `embedded_skills_found_with_empty_filesystem_dirs`
- `filesystem_skill_overrides_embedded_builtin`
- `extract_builtin_references_writes_to_shared_dir`
- `extract_builtin_references_is_idempotent`
- (In `tests.rs`): `extract_builtin_references_writes_embedded_reference_files`,
  `discover_skills_includes_embedded_builtins`,
  `workspace_skill_overrides_embedded_builtin`

**Verification gate:** ✅ Browser automation skill is discovered from
`config/skills/BrowserAutomation/SKILL.md`, activates when shell is ready and
`PINCHTAB_URL` is set, renders with URL substitution, and appears in the system
prompt with all essential sections (Core Loop, Key Endpoints, Best Practices,
File Integration, If Blocked). Shell policy with browser overlay allows all
required browser commands (curl, jq, sleep, operators) while blocking
unauthorized commands. The skill is within the 3000-token cap.

### Phase D: Human-in-the-Loop (Post-MVP — separate detailed plan)

**Goal:** Agent can pause and request human help for browser blockers.

This is a dedicated tool (not a skill) because it needs gateway protocol
integration (pause/resume signaling) that cannot be done via shell.

> **Note:** This phase is intentionally kept at overview level. A separate
> detailed plan should be written after Phase C is complete and basic browser
> automation is validated in practice. Real-world usage will inform the exact
> UX requirements (timeout behavior, notification content, CDP connection
> flow, resume semantics).

**Scope (high-level):**
- `RequestHumanAssistanceTool` implementation
- Gateway `ResumeHumanAssistance` frame
- `/resume` command in TUI and Telegram
- Timeout handling
- CDP connection instructions in notifications

### Phase E: Future Skills

Examples of other skills that follow the same pattern — each is a markdown file
in `config/skills/`, requiring no code changes:

- **git-workflow.md** — Git conventions, PR templates, commit message format
- **code-review.md** — Review checklists, output format
- **data-analysis.md** — Python/pandas patterns for data tasks

---

## 7. Trade-off Analysis

### What we gain

| Dimension | Improvement |
|---|---|
| **Maintenance** | API changes → edit a `.md` file instead of Rust code |
| **Provider compatibility** | No tool schema = no provider-specific JSON schema bugs |
| **Flexibility** | LLM can use any Pinchtab endpoint immediately |
| **Debugging** | LLM sees raw HTTP responses, can inspect and adapt |
| **Extensibility** | New skills are `.md` files — no code, no compilation |
| **Reusability** | Can use Pinchtab's official skill/API docs directly |

### What we lose (and mitigations)

| Loss | Impact | Mitigation |
|---|---|---|
| Token efficiency | ~35 extra tokens per curl command (auth header, URL) | Pinchtab's batch `/actions` endpoint; `format=compact` snapshots; prompt caching |
| Implicit context tracking | LLM must track tab IDs in shell variables | Shell variables (`$TAB`) handle this; documented in skill |
| Pre-call validation | No structured validation before API call | Pinchtab returns clear HTTP errors; LLM handles them |
| Path safety for screenshots | No `validate_save_path` in Rust | Container sandbox enforces boundaries; Pinchtab has its own `SafePath` check |

### Token budget (typical 5-step browser task)

| Component | Skill approach | Dedicated tool |
|---|---|---|
| Skill/schema in prompt | ~1200 (cached) | ~400 (schema, cached) |
| Per-turn input (commands/actions) | ~250 | ~80 |
| Per-turn output (snapshots, text) | ~2500 | ~2500 |
| **5-step total** | **~3950** | **~2980** |

The ~170 extra input tokens per turn cost ~$0.01 over a 20-turn session.
Output tokens (snapshots, page text) dominate the budget and are identical.

---

## 8. Security Considerations

### Container isolation

All browser activity stays inside the sandboxed shell-vm container. The host
never runs Chrome. This is the same boundary as `shell_exec`.

### Auth token

`BRIDGE_TOKEN` prevents unauthorized access to Pinchtab. Even without a token,
Pinchtab binds to `127.0.0.1` — only processes on the same host can reach it.
The token adds defense-in-depth.

The token value is in the shell environment (`$BRIDGE_TOKEN`) but never appears
in the system prompt. The LLM references it as an env var in curl commands.

### Profile security

Profile directories contain cookies and session tokens. They live in
`/shared/.pinchtab/` inside the container — accessible to the agent (which
already has full workspace access via shell). This is the same trust boundary
as `shell_exec` access to workspace files.

### Network egress

With host networking, Chrome can reach any network endpoint the host can. This
is the same risk surface as `shell_exec` (where the agent can `curl` any URL).

---

## 9. Relationship to Guidebook

- **Chapter 14 (Productization)** describes skills as markdown manifests in
  managed folders. This plan implements a minimal version of that vision.
- **Chapter 15 (Phase 20)** envisions LLM-created skills. This plan builds the
  foundation (loading, activation, injection) without LLM authoring.
- **Chapter 4 (Tool System)** is unaffected. No tool trait changes needed.
- **Chapter 8 (Runner/Guests)** covers container setup; this plan adds Pinchtab
  as a sibling process in the shell-vm container.

---

## 10. Directory Structure (Post-Implementation)

```
config/
├── runner.toml
├── runner-user.toml
├── agent.toml
└── skills/
    └── BrowserAutomation/
        ├── SKILL.md
        └── references/
            └── pinchtab-api.md
docker/
├── Dockerfile
├── Dockerfile.prebuilt
└── shell-vm-entrypoint.sh
crates/
├── types/src/skill.rs       # Skill, SkillActivation, SkillMetadata
├── runner/src/bootstrap.rs  # Modified: skill injection in build_system_prompt()
└── runner/src/skills.rs     # SkillLoader
```
