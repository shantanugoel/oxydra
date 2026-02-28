# Chapter 13: Observability

> **Status:** Planned
> **Implemented:** `tracing` instrumentation throughout all crates (spans, structured events)
> **Remaining:** OpenTelemetry OTLP export, distributed traces, metrics pipeline, cost reporting, dashboard templates
> **Last verified against code:** 2026-02-28

## Overview

Oxydra is instrumented with `tracing` from the foundation layer upward. Every provider call, tool execution, turn transition, and gateway routing decision emits structured spans and events. This chapter describes the current instrumentation baseline and the planned upgrade to full OpenTelemetry export with distributed traces, metrics, and cost reporting.

## Current Instrumentation

### Tracing Spans

The runtime and gateway are instrumented with `#[instrument]` spans at key boundaries:

- **Turn-level** — turn number, state transitions (`Streaming` → `ToolExecution` → `Yielding`)
- **Provider call** — model identifier, request/response token counts, latency
- **Tool execution** — tool name, safety tier, execution duration, success/failure
- **Budget enforcement** — context utilization percentage, accumulated cost
- **Gateway routing** — session creation, turn dispatch, cancellation events
- **Memory operations** — store, recall, summarization triggers

These spans are emitted via `tracing-subscriber` and currently output to stderr as structured text logs. The instrumentation code itself does not need to change when upgrading to OpenTelemetry — only the subscriber configuration.

### Structured Events

Beyond spans, the runtime emits structured events for:
- Self-correction loops (tool validation failure → retry)
- Context window management (history truncation, summary injection)
- Credential scrubbing (redacted patterns detected)
- Connection state changes (provider stream drops, gateway reconnections)

## OpenTelemetry Integration

### Trace Export

The upgrade path from `tracing` to OpenTelemetry:

1. Add `opentelemetry` + `tracing-opentelemetry` as dependencies in `runtime` and `gateway`
2. Replace the stderr subscriber with an OpenTelemetry-aware subscriber pipeline
3. Configure OTLP export to Jaeger, Grafana Tempo, or any OTel-compatible backend

Existing `#[instrument]` annotations and `tracing::info!` / `tracing::error!` calls automatically become OTel spans and events — no code changes required in business logic.

### Distributed Traces

A single user request generates a trace tree:

```
User Turn
├── Gateway: route_turn (session lookup, validation)
├── Runtime: run_session_internal
│   ├── Memory: recall (hybrid search)
│   ├── Provider: stream (SSE connection, token accumulation)
│   │   └── HTTP: POST /v1/chat/completions
│   ├── Tool: execute (read_file)
│   ├── Tool: execute (write_file)
│   ├── Provider: stream (second turn after tools)
│   │   └── HTTP: POST /v1/chat/completions
│   ├── Scrubbing: scrub_tool_output
│   └── Memory: store (conversation events)
└── Gateway: send_turn_completed
```

With multi-agent orchestration (Chapter 11), trace context propagates through `SubagentBrief` handoffs so the full delegation tree is visible as a single trace.

### Metrics

Key metrics to export:

| Metric | Type | Labels | Purpose |
|--------|------|--------|---------|
| `oxydra_turn_duration_seconds` | Histogram | `user_id`, `model` | Turn latency distribution |
| `oxydra_provider_tokens_total` | Counter | `provider`, `model`, `direction` (input/output) | Token consumption tracking |
| `oxydra_provider_cost_usd` | Counter | `provider`, `model`, `user_id` | Cost attribution |
| `oxydra_tool_executions_total` | Counter | `tool_name`, `safety_tier`, `result` (ok/error) | Tool reliability |
| `oxydra_tool_duration_seconds` | Histogram | `tool_name` | Tool latency distribution |
| `oxydra_context_utilization_ratio` | Gauge | `user_id`, `model` | How full the context window is |
| `oxydra_active_sessions` | Gauge | — | Current session count |
| `oxydra_memory_chunks_total` | Gauge | `user_id` | Memory growth tracking |

Metrics are emitted via the OpenTelemetry metrics SDK alongside traces, using the same OTLP export pipeline.

## Cost Reporting

### Per-Session Cost Tracking

The runtime already tracks `accumulated_cost` per session for budget enforcement. With observability, this data becomes reportable:

- **Real-time cost** — current session cost is available as a metric and can be queried
- **Historical cost** — per-session and per-user cost aggregated from stored `UsageUpdate` events in memory
- **Model-level attribution** — cost broken down by model, useful when multi-agent orchestration routes to different models

### Cost Calculation

Token costs are computed using pricing data from the model catalog:

```
cost = (input_tokens * model.input_price_per_token)
     + (output_tokens * model.output_price_per_token)
```

The model catalog already stores pricing metadata per model. The runtime applies this after each provider response and emits it as both a budget check and a metric.

## Conversation Replay

Stored traces and conversation events enable replay capabilities:

- **Debug replay** — reconstruct the exact sequence of messages, tool calls, and responses for a session
- **Audit trail** — verify what the agent did, which tools it called, and what data it accessed
- **Performance analysis** — identify slow provider calls, expensive tool executions, or unnecessary turn iterations

Replay is built on the existing `conversation_events` table in memory, augmented with trace IDs linking each event to its OpenTelemetry span.

## Implementation Approach

Observability is implemented incrementally:

1. **OTel subscriber** — swap the stderr subscriber for `tracing-opentelemetry` in gateway and runtime startup
2. **Metrics pipeline** — add meter providers for the key metrics listed above
3. **Cost reporting** — surface accumulated cost data through the gateway's health/status endpoint and as OTel metrics
4. **Trace context propagation** — inject trace IDs into `SubagentBrief` for multi-agent trace continuity
5. **Dashboard templates** — provide Grafana/Jaeger dashboard configurations for common queries

## Configuration

Observability configuration follows the existing layered config system:

```toml
[observability]
enabled = false                    # opt-in, not default
exporter = "otlp"                  # "otlp", "stdout", "none"
otlp_endpoint = "http://localhost:4317"
service_name = "oxydra"

[observability.metrics]
enabled = true
export_interval_seconds = 30

[observability.traces]
enabled = true
sample_rate = 1.0                  # 1.0 = trace everything
```

The feature is gated behind a `gateway` feature flag so it doesn't add dependencies to minimal builds.

## Design Boundaries

- Instrumentation code is always present (it's just `tracing` macros) — only the export pipeline is optional
- No PII or credentials appear in trace spans — the same scrubbing rules from `runtime/src/scrubbing.rs` apply
- Trace spans are bounded in cardinality — per-token spans are not emitted (per-turn granularity is the minimum)
- Cost data is derived from the model catalog, not from provider billing APIs — it is an estimate, not a billing source
