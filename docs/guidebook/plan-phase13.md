Plan Overview
- 7 steps, clear dependencies, and verification gates
- Backward‑compatible provider config evolution
- New provider registry, Gemini provider, and OpenAI Responses API provider
- Deterministic model catalog snapshot governance
---
Step 1 — Enrich ModelDescriptor schema (foundation)
Goal: make the catalog a governance artifact (pricing, deprecation details, knowledge cutoff).
- Update crates/types/src/model.rs:
  - Add ModelPricing { input_per_million_tokens, output_per_million_tokens }
  - Add DeprecationInfo { deprecated_at, sunset_at, successor_model }
  - Extend ModelDescriptor with:
    - pricing: Option<ModelPricing>
    - deprecation_info: Option<DeprecationInfo>
    - knowledge_cutoff: Option<String>
- Update crates/types/data/pinned_model_catalog.json to include new optional fields for all existing models.
Verification
- ModelCatalog::from_pinned_snapshot() parses enriched JSON
- New test: enriched_model_descriptor_serialization_round_trip
---
Step 2 — Provider registry config types
Goal: transition from hardcoded provider blocks to a registry while staying backward compatible.
- Update crates/types/src/config.rs:
  - Add ProviderRegistryEntry:
    - provider_type, base_url, api_key, api_key_env, extra_headers
  - Extend ProviderConfigs:
    - Keep openai, anthropic
    - Add registry: BTreeMap<String, ProviderRegistryEntry>
  - Add ProviderConfigs::resolve(provider_name):
    - Prefer registry entry
    - Fallback to synthesized openai/anthropic
  - Update AgentConfig::validate() to use registry resolution
Recommended default: keep flat config fields for backward compat; registry is additive.
Verification
- flat_openai_config_synthesized_to_registry_entry
- registry_provider_overrides_flat_config
- unknown_registry_provider_rejected
---
Step 3 — Provider factory + generic API key resolution
Goal: replace hardcoded provider selection with a factory built from the registry.
- Update crates/provider/src/lib.rs:
  - resolve_api_key_for_entry(entry)
  - build_provider_from_entry(provider_name, entry)
- Update crates/tui/src/bootstrap.rs:
  - Replace hardcoded openai|anthropic match with the factory
- Default api_key_env values for synthesized entries:
  - OpenAI → OPENAI_API_KEY
  - Anthropic → ANTHROPIC_API_KEY
Verification
- factory_constructs_openai_from_registry_entry
- factory_constructs_anthropic_from_registry_entry
- api_key_resolved_from_entry_env_var
---
Step 4 — Gemini provider
Goal: add Gemini provider with configurable base URL.
- Add crates/provider/src/gemini.rs (new provider)
- Wire in crates/provider/src/lib.rs factory
- Add Gemini models to crates/types/data/pinned_model_catalog.json
Recommended defaults:
- provider_type: gemini
- base_url: https://generativelanguage.googleapis.com
- api_key_env: GEMINI_API_KEY
- Streaming: streamGenerateContent?alt=sse (SSE parser reuse)
Verification
- gemini_request_serialization_snapshot
- gemini_stream_chunk_parsing
- gemini_complete_round_trip (mock server)
---
Step 5 — OpenAI Responses API provider (Gap 5)
Goal: implement stateful /v1/responses provider with previous_response_id chaining.
- Add crates/provider/src/responses.rs
- Wire in factory in crates/provider/src/lib.rs
Key behavior:
- provider_type: openai-responses
- base_url: https://api.openai.com
- Uses previous_response_id stored in provider state (Arc<Mutex<Option<String>>>)
- Only send new messages since last response; fall back to full history if desync
SSE event mapping:
- response.output_text.delta → StreamItem::Text
- response.function_call_arguments.delta → StreamItem::ToolCallDelta
- response.completed → set previous_response_id, emit FinishReason
Verification
- responses_sse_event_parsing
- previous_response_id_updated_on_success
- partial_input_when_previous_response_set
- responses_stream_round_trip
---
Step 6 — Snapshot generation command
Goal: deterministic, reproducible catalog regeneration + CI verification.
- Update crates/runner/src/main.rs:
  - --regenerate-catalog
  - --verify-catalog
- Update crates/types/src/model.rs:
  - ModelCatalog::verify_snapshot()
Verification
- regenerate_snapshot_is_deterministic
- verify_snapshot_detects_drift
---
Step 7 — Validation + wiring + deprecation warnings
Goal: startup validation and consistent provider/model mapping.
- Update crates/tui/src/bootstrap.rs:
  - Emit deprecation warnings if model deprecated
  - Validate provider_type ↔ model.provider compatibility
- OpenAI Responses uses OpenAI catalog entries:
  - ResponsesProvider::provider_id() returns openai
Verification
- deprecated_model_emits_warning
- provider_type_model_mismatch_rejected
- openai_responses_provider_uses_openai_catalog
---
Dependency Graph
Step 1 → Step 2 → Step 3
                  ├─ Step 4 (Gemini)
                  └─ Step 5 (Responses)
Step 1 → Step 6 (needs Gemini models)
Step 7 depends on 1–6
---
Risks + Mitigations
- Backward‑compat config break: keep flat fields + registry additive
- Responses API desync: track last message count; fallback to full history
- openai-responses model mapping: treat as openai catalog provider
