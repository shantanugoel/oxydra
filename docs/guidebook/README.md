# Oxydra Guidebook

Internal engineering documentation for the Oxydra AI agent orchestrator. This guidebook describes the system as it is actually built — architecture, patterns, and implementation details derived from the codebase, not from aspirational planning documents.

## Chapters

### Implemented (Phases 1-12)

| # | Chapter | Description |
|---|---------|-------------|
| 1 | [Architecture Overview](01-architecture-overview.md) | Core philosophy, workspace layout, dependency hierarchy, key design decisions |
| 2 | [Configuration System](02-configuration-system.md) | Layered config with figment, credential resolution, runner config, validation |
| 3 | [Provider Layer](03-provider-layer.md) | Provider trait, OpenAI/Anthropic implementations, SSE streaming, model catalog, reliability wrapper |
| 4 | [Tool System](04-tool-system.md) | Tool trait, `#[tool]` procedural macro, core tools, validation, safety tiers |
| 5 | [Agent Runtime](05-agent-runtime.md) | Turn loop, state machine, tool dispatch, self-correction, budget enforcement |
| 6 | [Memory and Retrieval](06-memory-and-retrieval.md) | libSQL persistence, schema, hybrid retrieval (vector + FTS5), embeddings, summarization |
| 7 | [Security Model](07-security-model.md) | Isolation tiers, WASM capabilities, security policy, SSRF protection, credential scrubbing |
| 8 | [Runner and Guests](08-runner-and-guests.md) | Runner lifecycle, VM provisioning, shell daemon protocol, bootstrap envelope |
| 9 | [Gateway and Channels](09-gateway-and-channels.md) | Channel trait, gateway WebSocket server, TUI adapter, end-to-end message flow |
| 10 | [Testing and Quality](10-testing-and-quality.md) | Test strategy, mocking, snapshot testing, CI/CD pipeline |

### Forward-Looking (Phases 13-21)

| # | Chapter | Description |
|---|---------|-------------|
| 11 | [Multi-Agent Orchestration](11-multi-agent-orchestration.md) | Subagent delegation, state graphs, lane-based routing, session trees |
| 12 | [External Channels and Identity](12-external-channels-and-identity.md) | External channel adapters, sender authentication, session identity mapping |
| 13 | [Observability](13-observability.md) | OpenTelemetry traces, metrics, cost reporting, conversation replay |
| 14 | [Productization](14-productization.md) | Model catalog governance, session lifecycle, scheduler, skills, persona, MCP |

### Reference

| # | Chapter | Description |
|---|---------|-------------|
| 15 | [Progressive Build Plan](15-progressive-build-plan.md) | All 21 phases with status, identified gaps, forward plan, test strategy |

## Build Instructions

### Prerequisites

```sh
# Install the Rust toolchain (stable)
rustup toolchain install stable

# Required for the WASM guest module (crates/wasm-guest → crates/sandbox)
rustup target add wasm32-wasip1
```

The `wasm32-wasip1` target is needed because `crates/sandbox/build.rs` cross-compiles the WASM guest binary at build time. Without it, any crate that transitively depends on `sandbox` with the default `wasm-isolation` feature will fail to build.

### Building

```sh
cargo build                          # all crates, default features
cargo build -p sandbox               # sandbox only (wasm-isolation on by default)
cargo test                           # full test suite
cargo test -p sandbox --features wasm-isolation   # sandbox isolation tests only
cargo test -p sandbox --no-default-features       # host-only fallback path
```

### CI requirements

Add the following step before any `cargo build` or `cargo test` step:

```yaml
- name: Install wasm32-wasip1 target
  run: rustup target add wasm32-wasip1
```

## How to Use This Guidebook

- **New to the codebase?** Start with Chapter 1 (Architecture Overview) for the big picture, then read chapters relevant to your work area.
- **Implementing a feature?** Check the Progressive Build Plan (Chapter 15) to understand phase dependencies and verification gates.
- **Debugging?** Chapters 5 (Agent Runtime) and 9 (Gateway and Channels) document the end-to-end message flow.
- **Security review?** Chapter 7 (Security Model) covers isolation tiers, WASM capabilities, and threat mitigations.

## Conventions

- **Chapters 1-10** describe the system as built. Code references point to actual implementations.
- **Chapters 11-14** describe planned features grounded in implementation reality. They reference existing infrastructure that the features build upon.
- **Chapter 15** tracks the overall build plan with completion status and identified gaps.

## Canonical Source

This guidebook is the canonical living documentation for the project, reflecting the actual implementation.
