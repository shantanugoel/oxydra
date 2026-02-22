## Problem statement
Current catalog validation blocks OpenAI-compatible proxies (for example OpenRouter) from using models not present in the filtered pinned snapshot. We want strict validation by default, broader upstream coverage via a cached catalog, a safe fallback path, and a user escape hatch for brand-new models.

## Plan

- Catalog sources and size
  - Keep the compiled-in snapshot filtered (openai/anthropic/google) to avoid a binary size jump.
  - Allow runtime to use a full **unfiltered** upstream catalog fetched from models.dev and cached locally. The compiled-in version stays lean for major providers; the cached copy covers the full ecosystem for expansion.

- Runtime catalog resolution (strict-by-default)
  - Load catalog from `~/.config/oxydra/model_catalog.json`, else `.oxydra/model_catalog.json`.
    - User-level cache (`~/.config/oxydra/`) takes precedence over workspace (`.oxydra/`) because models can be updated in the world without updating the binary, and a user-level cache reflects the latest state regardless of workspace.
  - If no cached catalog exists, **auto-fetch** from upstream (`https://models.dev/api.json`) and write the cache to `~/.config/oxydra/model_catalog.json`. This keeps the default experience zero-config — users don't need to run a manual fetch step.
  - If fetch fails (network error, air-gapped environment), fall back to the compiled-in filtered snapshot.
  - If fetched JSON fails schema parse, print a clear "unsupported schema" error with source info and fall back to the next source (compiled-in snapshot).

- CLI changes
  - `oxydra catalog fetch` defaults to upstream (`https://models.dev/api.json`) and writes the **unfiltered** catalog to the cache location (`~/.config/oxydra/model_catalog.json`).
  - Add `--pinned` to fetch the pinned repo snapshot instead.
    - Default pinned URL: `https://raw.githubusercontent.com/shantanugoel/oxydra/refs/heads/main/crates/types/data/pinned_model_catalog.json`
    - The pinned URL is configurable via an optional config field (e.g. `catalog.pinned_url`) to allow forks or private mirrors to override it.
  - Keep unfiltered catalog when fetching upstream (no provider filtering applied to the cached copy).

- Validation behavior
  - Keep strict catalog validation by default.
  - Add a `skip_catalog_validation` flag (config field + CLI `--skip-catalog-validation` override) for users who want to try models not yet in the catalog.
  - When `skip_catalog_validation` is enabled:
    - Skip the `validate_model_in_catalog()` check entirely.
    - Skip the `capabilities()` validation call that would fail for unknown models.
    - Use sensible minimal defaults for `ProviderCaps` (e.g. `supports_streaming: true`, `supports_tools: true`, `supports_json_mode: false`, `supports_reasoning_traces: false`, no token limits).
    - Allow optional per-registry-entry config overrides for essential capability properties that cannot be inferred: `reasoning`, `max_input_tokens`, `max_output_tokens`, `max_context_tokens`. These override the defaults when the model is not in the catalog.
  - Validation continues to use catalog provider namespace (`catalog_provider`) and model id; registry key names remain user-defined and are not validated.

- Verify behavior
  - `oxydra catalog verify` checks against the **cached** catalog if one exists, otherwise against the compiled-in snapshot. Same resolution semantics as runtime loading.

- Docs/guidebook updates
  - Update `docs/guidebook/02-configuration-system.md` (catalog_provider semantics + skip validation flag + cache locations + optional capability overrides in registry entries + catalog config section with pinned_url).
  - Update `docs/guidebook/03-provider-layer.md` (catalog loading order, auto-fetch, unfiltered cache, schema mismatch behavior, default caps for unknown models).
  - Update `docs/guidebook/15-progressive-build-plan.md` (if it references fetch/verify behavior).
