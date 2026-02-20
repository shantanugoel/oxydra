#!/usr/bin/env bash
set -euo pipefail

MODE="${1:-local}"

case "$MODE" in
  local)
    cargo test -p runtime openai_compatible_runtime_e2e_exposes_tools_and_executes_loop -- --nocapture
    ;;
  live)
    : "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY (or export OPENAI_API_KEY)}"
    OPENROUTER_MODEL="${OPENROUTER_MODEL:-google/gemini-3-flash-preview}" \
      cargo test -p runtime live_openrouter_tool_call_smoke -- --ignored --nocapture
    ;;
  all)
    cargo test -p runtime openai_compatible_runtime_e2e_exposes_tools_and_executes_loop -- --nocapture
    : "${OPENROUTER_API_KEY:?set OPENROUTER_API_KEY (or export OPENAI_API_KEY)}"
    OPENROUTER_MODEL="${OPENROUTER_MODEL:-google/gemini-3-flash-preview}" \
      cargo test -p runtime live_openrouter_tool_call_smoke -- --ignored --nocapture
    ;;
  *)
    echo "Usage: $0 [local|live|all]" >&2
    exit 1
    ;;
esac
