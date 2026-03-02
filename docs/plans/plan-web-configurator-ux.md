# Web Configurator UX Overhaul — Detailed Implementation Plan

## Status

- **State:** In Progress (Phase 1 complete)
- **Issue:** Follow-up to [#7](https://github.com/shantanugoel/oxydra/issues/7)
- **Scope:** `runner` crate (backend API + frontend static files)
- **Prerequisite:** Web configurator V1 complete (plan-web-configurator.md)

---

## Problem Statement

The V1 web configurator works but provides a poor user experience:

1. **Not a true configurator** — It mirrors the current config file state. Users cannot discover config options that aren't already in their TOML file (e.g., adding new agents, providers, optional tool configs).
2. **No help text** — Field labels are raw dot-paths like `reliability.backoff_base_ms` with no description.
3. **Wrong input types** — Most fields are raw text inputs. Enums should be dropdowns, tool lists should be multi-selects, etc.
4. **JSON blobs for structured data** — Providers, agents, channels render as raw JSON textareas.
5. **No model catalog integration** — Provider/model selection is free-text with no knowledge of available models.
6. **No catalog refresh** — No UI to pull fresh model data from models.dev.
7. **Broken mobile nav** — Sidebar nav uses `overflow-x: auto` on mobile, hiding items without affordance (no hamburger menu).

## Goals

1. Transform config editors from generic field renderers to purpose-built structured forms.
2. Every field has a human-readable label, help text description, and appropriate input widget.
3. Enum fields use dropdowns. Multi-value fields use multi-select. Complex objects use structured sub-forms.
4. Model catalog is browsable; provider/model selection uses searchable dropdowns.
5. Users can add/remove dynamic entries (providers, agents, sender bindings, credential refs).
6. Catalog refresh is available from the UI.
7. Mobile nav uses a hamburger menu with slide-out overlay.
8. Backend serves as single source of truth for all field metadata, enum values, and dynamic lists.

## Non-Goals

1. Auto-generated forms from JSON Schema (forms remain hand-crafted for good UX).
2. Dark mode (still deferred to V2).
3. Changing the PATCH/merge-patch write semantics (they work fine).
4. Changing the security model or auth flows.

---

## Architecture

### New Backend Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/v1/meta/schema` | Returns field metadata for all config types: labels, descriptions, enum options, defaults, constraints, dynamic source references |
| `GET` | `/api/v1/catalog` | Returns the resolved model catalog (cache → pinned): providers with their models, capabilities, pricing |
| `GET` | `/api/v1/catalog/status` | Returns catalog source info: which file is loaded, last modified time, provider/model counts |
| `POST` | `/api/v1/catalog/refresh` | Triggers catalog fetch from models.dev (or pinned URL from config). Returns summary |

### Schema Metadata Design

The `/api/v1/meta/schema` endpoint returns a purpose-built metadata object (NOT JSON Schema). It is organized by config type and section:

- **Hybrid generation (recommended):**
  - **Auto-derived metadata:** defaults from typed config defaults, enum options from Rust enums, tool names from the tool registry/schema declarations, and dynamic providers from effective layered config loading (including `providers.toml`).
  - **Manually-authored metadata:** section grouping/order, labels, descriptions, input widgets, help text wording, and UX-specific visibility behavior.
  - **Explicitly not from Figment introspection:** Figment is the correct merge/extract mechanism for effective config values, but it does not provide rich, reliable field-level UI metadata for form generation.

```json
{
  "agent": {
    "sections": [
      {
        "id": "selection",
        "label": "Model Selection",
        "description": "Choose the LLM provider and model for agent conversations",
        "fields": [
          {
            "path": "selection.provider",
            "label": "Provider",
            "description": "The provider registry name to use for LLM API calls",
            "input_type": "select_dynamic",
            "dynamic_source": "registered_providers",
            "default": "openai",
            "required": true,
            "allow_custom": true
          },
          {
            "path": "selection.model",
            "label": "Model",
            "description": "The model ID to use. Choose from the catalog or enter a custom model ID.",
            "input_type": "model_picker",
            "default": "gpt-4o-mini",
            "required": true,
            "allow_custom": true
          }
        ]
      }
    ]
  },
  "runner": { "sections": [...] },
  "user": { "sections": [...] },
  "dynamic_sources": {
    "registered_providers": ["openai", "anthropic", ...],
    "tool_names": ["file_read", "file_write", "shell_exec", ...],
    "timezone_suggestions": ["UTC", "America/New_York", "Europe/London", "Asia/Kolkata", ...]
  }
}
```

### Nullable Field Metadata

In addition to label/description/default/input type metadata, each field should carry a `nullable` flag indicating whether the field is `Option<T>` and can be unset. This is needed so the frontend knows which fields support an explicit "clear/unset" action that sends `null` in the merge patch. Secret fields already use the `__UNCHANGED__` sentinel; for other optional fields, `null` is the clear signal.

### Input Type Taxonomy

| `input_type` | Widget | Description |
|--------------|--------|-------------|
| `text` | `<input type="text">` | Free-text string |
| `number` | `<input type="number">` | Numeric input with optional min/max/step |
| `boolean` | Toggle switch | On/off toggle |
| `secret` | Password input + clear button | Masked field with `__UNCHANGED__` sentinel |
| `select` | `<select>` dropdown | Static enum with predefined options |
| `select_dynamic` | `<select>` dropdown | Options fetched from `dynamic_sources` |
| `model_picker` | Custom searchable dropdown | Model selection from catalog, grouped by provider |
| `multiline` | `<textarea>` | Multi-line text (system prompts) |
| `multi_select` | Checkbox list | Multiple selections from a list |
| `tag_list` | Tag input | Free-form list of strings (glob patterns, env keys) |
| `key_value_map` | Repeating key+value rows | Dynamic map entries (credential_refs, extra_headers) |
| `readonly` | Disabled text | Non-editable field (config_version) |

### Dynamic Collection Editors

For `providers.registry` (map), `agents` (map), `channels.telegram.senders` (array), and `credential_refs` (map), the frontend uses a **collection editor** pattern:

- Each entry is rendered as a collapsible card
- "Add" button creates a new entry with defaults
- "Remove" button deletes an entry (with confirmation)
- Map collections (`providers.registry`, `agents`, `credential_refs`) encode deletions via `null` merge-patch values.
- Array collections (`channels.telegram.senders`) are saved as full-array replacements (not sparse `null` tombstones).

---

## Complete Field Metadata Reference

### Agent Config (`agent.toml`)

#### Section: General
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `config_version` | Config Version | Agent configuration schema version | `readonly` | `1.0.0` | — |

#### Section: Model Selection (`selection`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `selection.provider` | Provider | Provider registry name for LLM API calls | `select_dynamic` (registered_providers) | `openai` | Required, allow custom |
| `selection.model` | Model | Model ID for LLM API calls | `model_picker` | `gpt-4o-mini` | Required, allow custom |

#### Section: Runtime Limits (`runtime`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `runtime.turn_timeout_secs` | Turn Timeout (seconds) | Maximum time for a single LLM turn before timeout | `number` | `60` | min: 1 |
| `runtime.max_turns` | Max Turns | Maximum agentic loop iterations per session | `number` | `8` | min: 1 |
| `runtime.max_cost` | Max Cost ($) | Maximum USD cost per session. Empty = no limit. | `number` | — | min: 0, optional |

#### Section: Context Budget (`runtime.context_budget`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `runtime.context_budget.trigger_ratio` | Trigger Ratio | Context usage ratio that triggers summarization (e.g., 0.85 = 85% full) | `number` | `0.85` | min: 0, max: 1, step: 0.05 |
| `runtime.context_budget.safety_buffer_tokens` | Safety Buffer (tokens) | Tokens reserved as safety margin when computing context budget | `number` | `1024` | min: 1 |
| `runtime.context_budget.fallback_max_context_tokens` | Fallback Max Context | Context window size fallback when model catalog has no data | `number` | `128000` | min: 1 |

#### Section: Summarization (`runtime.summarization`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `runtime.summarization.target_ratio` | Target Ratio | How much to compress context during summarization (0.0-1.0) | `number` | `0.5` | min: 0, max: 1, step: 0.05 |
| `runtime.summarization.min_turns` | Min Turns | Minimum conversation turns before summarization can trigger | `number` | `6` | min: 1 |

#### Section: Memory (`memory`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `memory.enabled` | Enable Memory | Enable persistent conversational memory | `boolean` | `false` | — |
| `memory.remote_url` | Remote URL | URL for remote memory storage. Leave empty for local storage. | `text` | — | optional |
| `memory.auth_token` | Auth Token | Authentication token for remote memory server | `secret` | — | Required when remote_url is set |
| `memory.embedding_backend` | Embedding Backend | Backend for generating vector embeddings | `select` | `model2vec` | Options: model2vec, deterministic |
| `memory.model2vec_model` | Model2Vec Model | Model size for Model2Vec embeddings | `select` | `potion_32m` | Options: potion_8m, potion_32m |

#### Section: Memory Retrieval (`memory.retrieval`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `memory.retrieval.top_k` | Top K Results | Number of most relevant memory entries to retrieve | `number` | `8` | min: 1 |
| `memory.retrieval.vector_weight` | Vector Weight | Weight for vector similarity search (0.0-1.0). Vector + FTS must sum to 1.0. | `number` | `0.7` | min: 0, max: 1, step: 0.1 |
| `memory.retrieval.fts_weight` | Full-Text Weight | Weight for full-text search (0.0-1.0). Vector + FTS must sum to 1.0. | `number` | `0.3` | min: 0, max: 1, step: 0.1 |

#### Section: Providers (`providers`)

This is a **collection editor** (dynamic map of `ProviderRegistryEntry`). Each entry:

| Path (relative to entry) | Label | Description | Input Type | Default | Constraints |
|--------------------------|-------|-------------|------------|---------|-------------|
| *(key)* | Provider Name | Unique name for this provider configuration | `text` | — | Required, used as registry key |
| `provider_type` | Provider Type | LLM API protocol implementation | `select` | `openai` | Options: openai, anthropic, gemini, openai_responses |
| `base_url` | Base URL | Custom API base URL. Leave empty for provider default. | `text` | — | optional |
| `api_key` | API Key | Direct API key. Prefer environment variable instead. | `secret` | — | optional |
| `api_key_env` | API Key Env Variable | Environment variable name containing the API key | `text` | varies | optional |
| `extra_headers` | Extra Headers | Additional HTTP headers sent with every request | `key_value_map` | — | optional |
| `catalog_provider` | Catalog Provider | Model catalog namespace for validation. Auto-detected from provider type. | `text` | auto | optional |
| `attachment` | Supports Attachments | Override: whether this provider handles file/image attachments | `boolean` | — | optional |
| `input_modalities` | Input Modalities | Override: accepted input types | `multi_select` | — | Options: image, audio, video, pdf, document. Optional. |
| `reasoning` | Supports Reasoning | Override: whether models support reasoning/thinking | `boolean` | — | optional |
| `max_input_tokens` | Max Input Tokens | Override: maximum input token limit | `number` | — | optional, min: 1 |
| `max_output_tokens` | Max Output Tokens | Override: maximum output token limit | `number` | — | optional, min: 1 |
| `max_context_tokens` | Max Context Tokens | Override: maximum context window size | `number` | — | optional, min: 1 |

**Note:** The advanced override fields (attachment, input_modalities, reasoning, max_*_tokens) are for custom/proxy providers with models not in the catalog. These should be shown in a collapsible "Advanced Overrides" sub-section.

#### Section: Reliability (`reliability`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `reliability.max_attempts` | Max Retry Attempts | Number of retry attempts for failed API calls | `number` | `3` | min: 1 |
| `reliability.backoff_base_ms` | Backoff Base (ms) | Base delay for exponential backoff | `number` | `250` | min: 1 |
| `reliability.backoff_max_ms` | Backoff Max (ms) | Maximum backoff delay. Must be ≥ base. | `number` | `2000` | min: 1 |
| `reliability.jitter` | Enable Jitter | Add random jitter to backoff to prevent thundering herd | `boolean` | `false` | — |

#### Section: Catalog (`catalog`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `catalog.skip_catalog_validation` | Skip Catalog Validation | Allow models not found in the catalog. Enable for custom/proxy providers. | `boolean` | `false` | — |
| `catalog.pinned_url` | Pinned Catalog URL | URL for fetching the model catalog snapshot. Leave empty for default. | `text` | — | optional |

#### Section: Tools — Web Search (`tools.web_search`)

This entire section is optional. Show an "Enable Web Search Config" toggle to create/remove it.

| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `tools.web_search.provider` | Search Provider | Web search backend | `select` | — | Options: duckduckgo, google, searxng. Optional. |
| `tools.web_search.base_url` | Base URL | Custom base URL for the search provider | `text` | — | optional |
| `tools.web_search.base_urls` | Fallback URLs | Comma-separated fallback base URLs | `text` | — | optional |
| `tools.web_search.api_key_env` | API Key Env | Env var name for Google API key | `text` | — | optional |
| `tools.web_search.engine_id_env` | Engine ID Env | Env var name for Google Custom Search engine ID | `text` | — | optional |
| `tools.web_search.query_params` | Extra Query Params | Additional parameters as `key=value&key2=value2` | `text` | — | optional |
| `tools.web_search.engines` | SearxNG Engines | Comma-separated SearxNG search engines | `text` | — | optional |
| `tools.web_search.categories` | SearxNG Categories | Comma-separated SearxNG categories | `text` | — | optional |
| `tools.web_search.safesearch` | Safe Search Level | SearxNG safe search level | `select` | — | Options: 0 (off), 1 (moderate), 2 (strict). Optional. |
| `tools.web_search.egress_allowlist` | Egress Allowlist | Hosts allowed to resolve to private/loopback addresses | `tag_list` | — | optional |

#### Section: Tools — Shell (`tools.shell`)

This entire section is optional. Show an "Enable Shell Config" toggle.

| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `tools.shell.allow` | Allow Commands | Additional commands/patterns to add to the allowlist | `tag_list` | — | optional |
| `tools.shell.deny` | Deny Commands | Commands/patterns to remove from the allowlist (deny wins) | `tag_list` | — | optional |
| `tools.shell.replace_defaults` | Replace Default Allowlist | If enabled, `allow` replaces the built-in list instead of extending it | `boolean` | `false` | optional |
| `tools.shell.allow_operators` | Allow Shell Operators | Allow shell control operators (&&, \|\|, \|, etc.) | `boolean` | `false` | optional |
| `tools.shell.env_keys` | Environment Variables | Env var names to forward into the shell container | `tag_list` | — | optional |

#### Section: Tools — Attachment Save (`tools.attachment_save`)

Optional section.

| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `tools.attachment_save.timeout_secs` | Save Timeout (seconds) | Timeout for file save operations | `number` | `60` | min: 1 |

#### Section: Scheduler (`scheduler`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `scheduler.enabled` | Enable Scheduler | Enable scheduled/recurring task execution | `boolean` | `false` | — |
| `scheduler.poll_interval_secs` | Poll Interval (seconds) | How often the executor checks for due schedules | `number` | `15` | min: 1 |
| `scheduler.max_concurrent` | Max Concurrent | Maximum simultaneous scheduled executions | `number` | `2` | min: 1 |
| `scheduler.max_schedules_per_user` | Max Schedules Per User | Maximum number of schedules a user can create | `number` | `50` | min: 1 |
| `scheduler.max_turns` | Max Turns Per Run | Maximum turns for each scheduled execution | `number` | `10` | min: 1 |
| `scheduler.max_cost` | Max Cost Per Run ($) | Maximum cost per scheduled execution | `number` | `0.50` | min: 0 |
| `scheduler.max_run_history` | Max Run History | History entries to keep per schedule | `number` | `20` | min: 1 |
| `scheduler.min_interval_secs` | Min Interval (seconds) | Minimum time between runs (anti-abuse) | `number` | `60` | min: 1 |
| `scheduler.default_timezone` | Default Timezone | Timezone for cron schedule interpretation | `text` | `Asia/Kolkata` | With suggestions |
| `scheduler.auto_disable_after_failures` | Auto-Disable After Failures | Consecutive failures before auto-disabling a schedule | `number` | `5` | min: 1 |
| `scheduler.notify_after_failures` | Notify After Failures | Consecutive failures before notifying the user. 0 = disabled. | `number` | `3` | min: 0 |

#### Section: Gateway (`gateway`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `gateway.max_sessions_per_user` | Max Sessions | Maximum concurrent sessions per user | `number` | `50` | min: 1 |
| `gateway.max_concurrent_turns_per_user` | Max Concurrent Turns | Maximum simultaneous turns per user | `number` | `10` | min: 1 |
| `gateway.session_idle_ttl_hours` | Session Idle TTL (hours) | Hours before an idle session is cleaned up | `number` | `48` | min: 1 |

#### Section: Agent Definitions (`agents`)

This is a **collection editor** (dynamic map of `AgentDefinition`). The key is the agent name. Each entry:

| Path (relative to entry) | Label | Description | Input Type | Default | Constraints |
|--------------------------|-------|-------------|------------|---------|-------------|
| *(key)* | Agent Name | Unique identifier for this agent definition | `text` | — | Required, no spaces |
| `system_prompt` | System Prompt | Custom system prompt text. Overrides default agent behavior. | `multiline` | — | optional |
| `system_prompt_file` | System Prompt File | Path to a file containing the system prompt. Alternative to inline. | `text` | — | optional |
| `selection.provider` | Provider Override | Use a different provider for this agent | `select_dynamic` (registered_providers) | inherits root | optional, allow custom |
| `selection.model` | Model Override | Use a different model for this agent | `model_picker` | inherits root | optional, allow custom |
| `tools` | Allowed Tools | Restrict this agent to specific tools. Empty = all tools. | `multi_select` (tool_names) | all | optional |
| `max_turns` | Max Turns Override | Override the global max turns for this agent | `number` | inherits root | optional, min: 1 |
| `max_cost` | Max Cost Override ($) | Override the global max cost for this agent | `number` | inherits root | optional, min: 0 |

---

### Runner Config (`runner.toml`)

#### Section: General
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `config_version` | Config Version | Runner configuration schema version | `readonly` | `1.0.1` | — |
| `workspace_root` | Workspace Root | Directory for user workspace data | `text` | `.oxydra/workspaces` | Required |
| `default_tier` | Default Sandbox Tier | Default isolation level for new users | `select` | `micro_vm` | Options: micro_vm, container, process |

#### Section: Guest Images (`guest_images`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `guest_images.oxydra_vm` | Oxydra VM Image | Container/VM image for the oxydra runtime | `text` | `oxydra-vm:latest` | Required |
| `guest_images.shell_vm` | Shell VM Image | Container/VM image for shell execution | `text` | `shell-vm:latest` | Required |
| `guest_images.firecracker_oxydra_vm_config` | Firecracker Oxydra Config | JSON config file path for microvm (Linux only) | `text` | — | optional |
| `guest_images.firecracker_shell_vm_config` | Firecracker Shell Config | JSON config file path for microvm (Linux only) | `text` | — | optional |

#### Section: Web Configurator (`web`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `web.enabled` | Enable Web Configurator | Whether the web UI is available | `boolean` | `true` | — |
| `web.bind` | Bind Address | IP address and port for the web server (e.g., 127.0.0.1:9400) | `text` | `127.0.0.1:9400` | Required, valid socket addr |
| `web.auth_mode` | Authentication Mode | How to protect the web interface | `select` | `disabled` | Options: disabled (no auth, loopback only), token (bearer token required) |
| `web.auth_token_env` | Auth Token Env Variable | Environment variable containing the bearer token | `text` | — | Required when auth_mode = token (if no inline token) |
| `web.auth_token` | Auth Token (inline) | Inline bearer token. Prefer using an environment variable. | `secret` | — | optional |

#### Section: Users (`users`)

Managed by the Users page (add/remove user registration). Not rendered as raw fields in the Runner config editor.

---

### User Config (per-user TOML)

#### Section: Mount Paths (`mounts`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `config_version` | Config Version | User configuration schema version | `readonly` | `1.0.1` | — |
| `mounts.shared` | Shared Mount | Path for shared workspace data | `text` | auto-resolved | optional |
| `mounts.tmp` | Temp Mount | Path for temporary files | `text` | auto-resolved | optional |
| `mounts.vault` | Vault Mount | Path for secure credential storage | `text` | auto-resolved | optional |

#### Section: Resource Limits (`resources`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `resources.max_vcpus` | Max vCPUs | Maximum virtual CPU cores. Empty = no limit. | `number` | — | optional, min: 1 |
| `resources.max_memory_mib` | Max Memory (MiB) | Maximum memory in mebibytes. Empty = no limit. | `number` | — | optional, min: 1 |
| `resources.max_processes` | Max Processes | Maximum concurrent processes. Empty = no limit. | `number` | — | optional, min: 1 |

#### Section: Credential References (`credential_refs`)

**Collection editor** (key-value map with secret values):

| Field | Label | Description | Input Type |
|-------|-------|-------------|------------|
| *(key)* | Credential Name | Identifier for this credential | `text` |
| *(value)* | Credential Value | The credential secret value | `secret` |

#### Section: Behavior Overrides (`behavior`)
| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `behavior.sandbox_tier` | Sandbox Tier Override | Override the default sandbox tier for this user | `select` | — | Options: micro_vm, container, process. Optional (empty = use runner default). |
| `behavior.shell_enabled` | Shell Access | Whether this user's agent can use the shell tool | `boolean` | — | optional (empty = use default) |
| `behavior.browser_enabled` | Browser Access | Whether this user's agent can use browser tools | `boolean` | — | optional (empty = use default) |

#### Section: Channels — Telegram (`channels.telegram`)

This entire section is optional. Show "Enable Telegram Channel" toggle.

| Path | Label | Description | Input Type | Default | Constraints |
|------|-------|-------------|------------|---------|-------------|
| `channels.telegram.enabled` | Enabled | Activate the Telegram channel adapter | `boolean` | `false` | — |
| `channels.telegram.bot_token_env` | Bot Token Env | Environment variable name holding the Telegram bot token | `text` | — | optional |
| `channels.telegram.polling_timeout_secs` | Polling Timeout (seconds) | Long-polling timeout for Telegram updates | `number` | `30` | min: 1 |
| `channels.telegram.max_message_length` | Max Message Length | Maximum characters before splitting a Telegram message | `number` | `4096` | min: 1 |

**Senders** (array of `SenderBinding`):

Each sender binding is a card with:
| Field | Label | Description | Input Type |
|-------|-------|-------------|------------|
| `platform_ids` | Platform IDs | Telegram user ID strings for this sender | `tag_list` |
| `display_name` | Display Name | Human-readable name for logging | `text` | optional |

---

## Implementation Phases

### Phase 1: Backend — Schema Metadata & Catalog Endpoints ✅

**Goal:** Backend serves all metadata the frontend needs for smart forms.

**Status:** Complete

**Steps:**

1. ✅ **Create `runner/src/web/schema.rs`** — Schema metadata endpoint:
   - Define `ConfigSchema`, `SchemaSection`, `SchemaField` structs with all metadata (label, description, input_type, default, constraints, enum_options, dynamic_source, `nullable`).
   - Implement `build_agent_schema()`, `build_runner_schema()`, `build_user_schema()` with a **hybrid strategy**: auto-derive structural/default/enum metadata from Rust types and overlay manually-authored UX metadata.
   - Build `dynamic_sources` including:
     - `registered_providers`: loaded from effective layered agent config (same loader path used by runtime, including `providers.toml`)
     - `tool_names`: derived from tool declarations (registry/schemas) so names stay in sync with actual registered tools
     - `timezone_suggestions`: curated list of common IANA timezones
     - `provider_types`: the ProviderType enum variants
     - `sandbox_tiers`: the SandboxTier enum variants
     - `auth_modes`: the WebAuthMode enum variants
     - `embedding_backends`: MemoryEmbeddingBackend variants
     - `model2vec_models`: Model2vecModel variants
     - `web_search_providers`: duckduckgo, google, searxng
     - `input_modalities`: image, audio, video, pdf, document
     - `safesearch_levels`: 0, 1, 2
   - `GET /api/v1/meta/schema` handler that returns the complete schema.

2. ✅ **Create `runner/src/web/catalog_api.rs`** — Catalog endpoints:
   - `GET /api/v1/catalog` — Loads model catalog (cache → pinned), returns providers with their models. Only returns fields relevant to the UI (id, name, family, reasoning, tool_call, attachment, modalities, cost, limit).
   - `GET /api/v1/catalog/status` — Returns: source (cache path or "pinned"), last_modified (file mtime if cached), provider_count, model_count.
   - `POST /api/v1/catalog/refresh` — Calls `catalog::run_fetch()` (spawned on blocking pool since it uses reqwest::blocking). Returns updated catalog summary. Accepts optional `{ "pinned": bool }` body.

3. ✅ **Wire new routes in `web/mod.rs`**:
   - Add the 4 new endpoints to the router.

4. ✅ **Add tool names source of truth** — Expose a helper in `tools`/`runner` that returns canonical tool names from actual tool declarations (not a duplicated hardcoded list in the web module). The `tools` crate already defines constants like `FILE_READ_TOOL_NAME`, `WEB_SEARCH_TOOL_NAME`, etc. — collect these into a single function so the schema endpoint and tool registration stay in sync.

**Verification gate:**
- ✅ `GET /api/v1/meta/schema` returns complete field metadata for all 3 config types.
- ✅ `GET /api/v1/catalog` returns provider/model data.
- ✅ `POST /api/v1/catalog/refresh` successfully fetches and caches a new catalog.
- ✅ Dynamic sources include current registered providers and all tool names.
- ✅ All existing tests pass, no clippy warnings.

---

### Phase 2: Frontend Infrastructure — Form System & Responsive Nav

**Goal:** Build the reusable form rendering infrastructure and fix mobile navigation.

**Steps:**

1. **Create `static/js/form-renderer.js`** — Reusable field rendering logic:
   - `renderField(field, schema, dynamicSources, onChange)` — Returns the appropriate input widget based on `schema.input_type`.
   - Implements all input types from the taxonomy: `text`, `number`, `boolean`, `select`, `select_dynamic`, `model_picker`, `multiline`, `multi_select`, `tag_list`, `key_value_map`, `readonly`.
   - Add nullable/unset handling for optional fields (optional strings/numbers + tri-state optional booleans).
   - Each field includes: label, description (as help text below the input or tooltip), the input widget, clear/unset control (when nullable), validation error display.
   - The `model_picker` widget: searchable dropdown grouped by provider, shows model name + context window + capabilities badges.
   - The `tag_list` widget: text input where pressing Enter/comma adds a tag. Tags are shown as chips with X to remove.
   - The `key_value_map` widget: repeating rows of key + value inputs with + and − buttons.
   - The `multi_select` widget: scrollable checkbox list with "select all / none" toggle.

2. **Create `static/js/collection-editor.js`** — Dynamic collection management:
   - `CollectionEditor` component for map-type collections (providers, agents, credential_refs).
   - Each entry rendered as a collapsible card with:
     - Header showing the entry key/name
     - Edit/Delete buttons
     - Expansion reveals the entry's form fields
   - "Add New" button with a modal/inline form for the entry key
   - For map collections, deletion sets the key to `null` in the patch (merge patch deletion)
   - For array collections, edits/deletions are emitted as full-array replacement payloads
   - Rename support (delete old key + create new)

3. **Create `static/js/section-renderer.js`** — Section rendering:
   - Renders a schema section as a collapsible card
   - Section header with title + description + expand/collapse toggle
   - Contains rendered fields from `form-renderer.js`
   - Tracks changes per section

4. **Update `static/css/style.css`** — Mobile hamburger menu:
   - Add hamburger button (visible only on mobile)
   - Sidebar becomes a slide-out overlay on mobile with backdrop
   - Close on navigation or backdrop click
   - Smooth transition animation
   - Update all existing responsive breakpoints
   - Add styles for new input types (tag-list chips, model picker, multi-select checkboxes)

5. **Update `static/index.html`** — Add hamburger button:
   - Add hamburger toggle button to the mobile nav area
   - Wire Alpine.js state for sidebar open/close
   - Load new JS modules

**Verification gate:**
- Hamburger menu works on mobile viewports (< 920px)
- Sidebar slides out and closes properly
- All input type widgets render correctly (manually tested with mock data)
- Existing pages still function (backward compatible)
- No console errors

---

### Phase 3: Agent Config Editor Rewrite

**Goal:** Transform the Agent Config page from generic fields to a structured, section-based form.

**Steps:**

1. **Fetch schema and catalog on page load**:
   - When navigating to `#/config/agent`, fetch:
     - `GET /api/v1/meta/schema` (cached after first load)
     - `GET /api/v1/config/agent` (current config values)
     - `GET /api/v1/catalog` (cached after first load, used by model_picker)
   - Merge schema + values to produce the editor state

2. **Render the Selection section**:
   - Provider dropdown populated from `dynamic_sources.registered_providers`
   - Model picker populated from catalog, filtered to the selected provider's `catalog_provider`
   - When provider changes, model picker updates available models
   - "Custom" option in both dropdowns for free-text entry

3. **Render Runtime / Context Budget / Summarization sections**:
   - All number fields with proper constraints
   - Help text below each field

4. **Render Memory section**:
   - `enabled` toggle
   - Conditional fields: `remote_url` and `auth_token` shown when memory is enabled
   - `embedding_backend` dropdown
   - `model2vec_model` dropdown (shown only when backend = model2vec)
   - Retrieval sub-section with weight fields

5. **Render Providers section** (collection editor):
   - Each provider shown as an expandable card
   - Card header: provider name + provider type badge
   - Card body: structured form with all fields
   - "Advanced Overrides" collapsible sub-section for attachment/modalities/reasoning/token limits
   - "Add Provider" button
   - "Remove Provider" button with confirmation

6. **Render Reliability section**:
   - Simple form fields with constraints

7. **Render Catalog section**:
   - `skip_catalog_validation` toggle
   - `pinned_url` text field
   - **Catalog status card**: shows current catalog source, provider/model counts
   - **"Refresh Catalog"** button that calls `POST /api/v1/catalog/refresh`

8. **Render Tools sections**:
   - Each tool config (web_search, shell, attachment_save) as an optional collapsible section
   - "Enable" toggle that creates/removes the section
   - When enabled, shows the structured fields

9. **Render Scheduler section**:
   - `enabled` toggle
   - All fields shown when enabled
   - Timezone field with suggestions

10. **Render Gateway section**:
    - Simple form fields

11. **Render Agents section** (collection editor):
    - Each agent as an expandable card
    - System prompt as a textarea
    - Optional selection override with model picker
    - Tool allowlist as multi-select checklist
    - Optional max_turns and max_cost
    - "Add Agent" button
    - "Remove Agent" button with confirmation

12. **Rework save flow**:
    - Build the JSON merge patch from changed fields across all sections, including explicit `null` for user-cleared nullable values
    - Validate → preview changes → PATCH
    - Handle collection additions/removals correctly in the patch (map deletion via `null`, array edits via full-array replacement)

**Verification gate:**
- All Agent Config fields are rendered with proper labels, help text, and input types
- Provider addition/removal works and saves correctly
- Agent definition addition/removal works and saves correctly
- Model picker shows catalog models and allows custom entry
- Tool multi-select shows all available tools
- Saving preserves TOML comments
- Round-trip: load → edit → save → reload → verify changes
- All existing tests pass

---

### Phase 4: Runner Config Editor Rewrite

**Goal:** Transform the Runner Config page to structured forms.

**Steps:**

1. **Render General section**:
   - `config_version` as readonly
   - `workspace_root` as text
   - `default_tier` as dropdown

2. **Render Guest Images section**:
   - Standard text fields with descriptions
   - Firecracker config paths as optional fields

3. **Render Web Configurator section**:
   - `enabled` toggle
   - `bind` text field
   - `auth_mode` dropdown (disabled/token)
   - Conditional fields: `auth_token_env` and `auth_token` shown only when auth_mode = token
   - Warning banner when auth_mode = disabled and bind address is not loopback

4. **Users section**: Show a link to the Users page instead of raw editing. Users are managed by the dedicated Users page.

**Verification gate:**
- All Runner Config fields have proper labels, help text, and input types
- `auth_mode` dropdown works with conditional fields
- `default_tier` dropdown works
- Save/reload round-trip works
- All existing tests pass

---

### Phase 5: User Config Editor Rewrite

**Goal:** Transform the User Config page to structured forms.

**Steps:**

1. **Render Mounts section**:
   - Optional text fields for shared/tmp/vault paths

2. **Render Resources section**:
   - Optional number fields for vCPUs/memory/processes

3. **Render Credential References** (collection editor):
   - Key-value map with secret masking on values
   - Add/remove credential entries

4. **Render Behavior Overrides section**:
   - `sandbox_tier` as optional dropdown (with "Use runner default" empty option)
   - `shell_enabled` as optional boolean (three-state: unset/true/false)
   - `browser_enabled` as optional boolean (three-state: unset/true/false)

5. **Render Channels section**:
   - Telegram as optional sub-section with "Enable" toggle
   - When enabled: structured form for all Telegram fields
   - Senders as a collection editor (array of sender bindings)
   - Each sender: platform_ids as tag list, display_name as text

**Verification gate:**
- All User Config fields have proper labels, help text, and input types
- Credential refs addition/removal works with secret masking
- Telegram channel enable/disable creates/removes config correctly
- Sender binding addition/removal works
- Save/reload round-trip works
- All existing tests pass

---

### Phase 6: Polish, Testing & Documentation

**Goal:** Production quality, comprehensive tests, documentation.

**Steps:**

1. **Schema endpoint tests**:
   - Verify all sections and fields are present for each config type
   - Verify dynamic sources are populated correctly
   - Verify enum options match Rust type definitions
   - Verify recursive field-path coverage (not just top-level keys)
   - Verify nullable metadata is present for all `Option<T>` fields

2. **Catalog endpoint tests**:
   - Verify catalog returns correct data from pinned snapshot
   - Verify refresh endpoint works (may need to mock network in tests)
   - Verify status endpoint returns correct source info
   - Verify concurrent refresh requests are serialized safely (no cache corruption)

3. **Frontend cross-browser testing**:
   - Verify all input types work in Chrome, Firefox, Safari
   - Verify mobile hamburger menu on iOS/Android browsers

4. **Accessibility improvements**:
   - All form fields properly labeled (aria-label or associated label)
   - Help text connected via aria-describedby
   - Keyboard navigation through all inputs
   - Focus management on section expand/collapse

5. **Empty state improvements**:
   - When agent.toml doesn't exist, show all sections with defaults and clear "not yet created" indication
   - Each optional section shows what enabling it does

6. **Documentation updates**:
   - Update guidebook with new API endpoints
   - Update web configurator section of guidebook with new UI capabilities

7. **Verify all quality gates**:
   - `cargo fmt --check` passes
   - `cargo clippy` with no warnings
   - `cargo nextest run` passes all tests
   - No `#[allow(clippy::...)]` directives added

**Verification gate:**
- All new endpoints have unit tests
- Schema metadata covers every config field path recursively (including nullable semantics)
- Mobile navigation tested at 320px, 375px, 768px viewports
- All form input types render and save correctly
- Map-vs-array merge patch semantics are validated in tests
- No regressions in existing web configurator functionality
- Documentation updated

---

## Dependency Policy

No new dependencies required. All existing dependencies (`axum`, `serde_json`, `toml_edit`, `rust-embed`, `mime_guess`) are sufficient.

The catalog refresh endpoint uses the existing `catalog::run_fetch()` which already depends on `reqwest` (blocking). For the async web handler, we'll spawn it on `tokio::task::spawn_blocking`.

---

## Frontend File Structure (Updated)

```
crates/runner/static/
  index.html                   # SPA shell (updated: hamburger button)
  js/
    app.js                     # Core app (updated: schema loading, hamburger state)
    form-renderer.js           # NEW: field rendering by input type
    collection-editor.js       # NEW: dynamic map/array collection management
    section-renderer.js        # NEW: collapsible section cards
    config-agent.js            # NEW: agent config structured editor
    config-runner.js           # NEW: runner config structured editor
    config-user.js             # NEW: user config structured editor
    catalog-picker.js          # NEW: model catalog browser/picker widget
    control.js                 # Existing (unchanged)
    onboarding.js              # Existing (unchanged)
    logs.js                    # Existing (unchanged)
    vendor/
      alpine.min.js            # Existing (unchanged)
  css/
    style.css                  # Updated: hamburger menu, new input styles
```

---

## Risks and Mitigations

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|------------|--------|------------|
| 1 | **Schema drift** — New config fields added in Rust but not in schema metadata | Certain (over time) | Medium | Add a recursive field-path coverage test (not top-level only) plus enum-option parity tests against Rust enums. |
| 2 | **Large page load** — Schema + catalog + config = multiple API calls on page load | Low | Low | Cache schema and catalog in Alpine.js app state after first fetch. Only re-fetch config on page navigation. |
| 3 | **Collection editor complexity** — Map add/remove with merge patch semantics is tricky | Medium | Medium | Thorough unit tests for patch generation from collection changes. Test add + edit + delete in same save operation. |
| 4 | **Model picker performance** — Large catalog (100+ models) may be slow in dropdown | Low | Low | Group by provider, virtual scrolling if needed. In practice, 3 providers × ~30 models = ~90 entries, which is fine. |
| 5 | **Hamburger menu z-index conflicts** — Toast notifications vs sidebar overlay | Low | Low | Ensure sidebar overlay has z-index below toasts. |
| 6 | **Optional section toggle semantics** — Creating/removing optional config sections (tools.web_search) must map to correct merge patch | Medium | Medium | When toggle off: send `{ "tools": { "web_search": null } }`. When toggle on: send section with defaults. Test round-trip. |
| 7 | **Nullable/unset semantic mismatch** — Optional fields cannot be reliably cleared or unset | Medium | Medium | Add explicit `nullable` schema metadata and round-trip tests for clear/unset behavior. |
| 8 | **Dynamic provider source mismatch** — Provider dropdown misses layered providers from `providers.toml` | Medium | Medium | Build provider dynamic source from effective layered config loader and add regression tests including `providers.toml`. |

---

## Acceptance Checklist

- [ ] `GET /api/v1/meta/schema` returns complete field metadata for agent, runner, and user configs.
- [ ] `GET /api/v1/catalog` returns model catalog with provider/model data.
- [ ] `POST /api/v1/catalog/refresh` fetches and caches updated catalog.
- [ ] Concurrent catalog refresh requests are handled safely.
- [ ] Every config field has a human-readable label and description.
- [ ] Enum fields use dropdown selects, not text inputs.
- [ ] Provider/model selection uses catalog-aware pickers.
- [ ] Provider dynamic options include entries from effective layered config sources (including `providers.toml`).
- [ ] Dynamic collections (providers, agents, credential refs, senders) support add/remove.
- [ ] Complex objects render as structured forms, not JSON textareas.
- [ ] Tool allowlists use multi-select from known tool names.
- [ ] Optional config sections have enable/disable toggles.
- [ ] Optional fields can be explicitly unset/cleared and persist correctly after reload.
- [ ] Map collections and array collections use correct, distinct merge-patch semantics.
- [ ] Mobile nav uses hamburger menu with slide-out sidebar.
- [ ] All responsive breakpoints work (320px → 1200px+).
- [ ] Config save round-trip works correctly for all config types.
- [ ] TOML comments preserved after structured form saves.
- [ ] All new backend code has unit tests.
- [ ] No clippy warnings, all tests pass.
- [ ] Documentation updated.
