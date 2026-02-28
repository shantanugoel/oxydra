# Oxydra Guidebook

Internal engineering documentation for the Oxydra AI agent orchestrator. This guidebook describes the system as it is actually built — architecture, patterns, and implementation details derived from the codebase, not from aspirational planning documents.

## Chapters

### Implemented (Phases 1-14, 18-19)

| # | Chapter | Description |
|---|---------|-------------|
| 1 | [Architecture Overview](01-architecture-overview.md) | Core philosophy, workspace layout, dependency hierarchy, key design decisions |
| 2 | [Configuration System](02-configuration-system.md) | Layered config with figment, credential resolution, runner config, validation |
| 3 | [Provider Layer](03-provider-layer.md) | Provider trait, OpenAI/Anthropic/Gemini/Responses implementations, SSE streaming, model catalog, capability overrides, reliability wrapper |
| 4 | [Tool System](04-tool-system.md) | Tool trait, `#[tool]` procedural macro, core tools, validation, safety tiers, LLM-callable memory tools, scheduler tools |
| 5 | [Agent Runtime](05-agent-runtime.md) | Turn loop, state machine, tool dispatch, self-correction, budget enforcement, scheduled turn execution |
| 6 | [Memory and Retrieval](06-memory-and-retrieval.md) | libSQL persistence, schema, hybrid retrieval (vector + FTS5), embeddings, summarization, note storage/deletion API, scheduler store |
| 7 | [Security Model](07-security-model.md) | Isolation tiers, WASM capabilities, security policy, SSRF protection, credential scrubbing |
| 8 | [Runner and Guests](08-runner-and-guests.md) | Runner lifecycle, VM provisioning, shell daemon protocol, bootstrap envelope |
| 9 | [Gateway and Channels](09-gateway-and-channels.md) | Channel trait, gateway WebSocket server, TUI adapter, end-to-end message flow, scheduled notifications |
| 10 | [Testing and Quality](10-testing-and-quality.md) | Test strategy, mocking, snapshot testing, CI/CD pipeline |

### Mixed Status (Phase 15 in progress; Phases 16-17, 20-21 planned)

| # | Chapter | Description |
|---|---------|-------------|
| 11 | [Multi-Agent Orchestration](11-multi-agent-orchestration.md) | In progress: subagent delegation and routing foundations are implemented; advanced orchestration graphs/session trees remain |
| 12 | [External Channels and Identity](12-external-channels-and-identity.md) | Implemented: external channel adapters, sender authentication, and durable session identity mapping |
| 13 | [Observability](13-observability.md) | OpenTelemetry traces, metrics, cost reporting, conversation replay |
| 14 | [Productization](14-productization.md) | Mixed: model catalog governance, session lifecycle, and scheduler are implemented; MCP/skills/persona remain planned |

### Reference

| # | Chapter | Description |
|---|---------|-------------|
| 15 | [Progressive Build Plan](15-progressive-build-plan.md) | All 21 phases with status, identified gaps, forward plan, test strategy |

## Build Instructions

### Prerequisites

```sh
# Install the Rust toolchain (stable)
rustup toolchain install stable

# Required for the WASM guest module (crates/wasm-guest → crates/tools)
rustup target add wasm32-wasip1
```

The `wasm32-wasip1` target is needed because `crates/tools/build.rs` cross-compiles the WASM guest binary at build time. Without it, any crate that transitively depends on `tools` with the default `wasm-isolation` feature will fail to build.

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
- **Chapters 11-14** are mixed-status chapters: Chapter 12 and parts of Chapters 11/14 are implemented, while Chapters 13 and remaining sections are forward-looking. Each of these chapters has a **status header** at the top indicating what is implemented vs. remaining.
- **Chapter 15** tracks the phase-by-phase completion status, in-progress work, and open gaps.
- **Status headers** — Mixed/planned chapters include a status block at the top with implementation status, last-verified date, and known gaps. When updating these chapters, keep the status header current.

## Cross-Document Consistency

The README (`README.md`) and this guidebook serve different audiences:

| Topic | Canonical Source | Other docs should... |
|-------|-----------------|---------------------|
| End-user commands, setup paths | `README.md` | Link to README, not duplicate |
| Runtime behavior defaults (queueing, limits, timeouts) | Guidebook chapter(s) | README references defaults but guidebook is authoritative |
| Feature maturity (implemented/experimental/planned) | Guidebook chapter status headers + Chapter 15 | README uses consistent wording |
| Provider/model support | Chapter 3 (Provider Layer) | README lists providers; chapter has implementation details |
| Configuration options | Chapter 2 (Configuration System) | README shows minimal examples; chapter is exhaustive |

**When making changes:** If a behavior default changes, update both the relevant guidebook chapter and the README. If a feature status changes, update the chapter status header and Chapter 15.

## Canonical Source

This guidebook is the canonical living documentation for the project, reflecting the actual implementation.
