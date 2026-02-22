# Chapter 1: Architecture Overview

## Core Philosophy

Oxydra is a high-performance AI agent orchestrator built in Rust, following the **minimal core** philosophy. The system deliberately restricts its built-in capabilities to a small set of primitives (file read/write/edit, shell execution), forcing the LLM to compose complex behaviors from these foundations rather than relying on hardcoded integrations. This keeps the core small, predictable, and extensible.

The key architectural principles:

- **Intentional reduction** — no bloated toolsets or protocol-specific integrations baked into the core runtime
- **Progressive construction** — each layer extends the previous without requiring rewrites
- **Strict crate boundaries** — enforced at the Cargo workspace level so the compiler prevents dependency violations
- **Local-first** — embedded databases, local embeddings, and single-binary deployment by default
- **Security by isolation** — OS-level sandboxing rather than reliance on LLM alignment

## Workspace Layout

```
crates/
  types/          # Foundation — zero internal dependencies
  provider/       # Core — LLM provider adapters
  tools/          # Core — tool trait + core tool implementations
  tools-macros/   # Core — #[tool] procedural macro
  runtime/        # Core — agent loop + state machine
  memory/         # Core — persistence + retrieval
  sandbox/        # Core — isolation + security policy
  runner/         # Application — host orchestrator
  shell-daemon/   # Application — guest shell/browser RPC
  channels/       # Application — channel adapters
  gateway/        # Application — routing daemon
  tui/            # Application — terminal channel UI
```

## Dependency Hierarchy

The workspace enforces a strict three-layer dependency graph. Lower layers never import from higher layers.

```
┌─────────────────────────────────────────────────────┐
│                   Application Layer                  │
│  runner, shell-daemon, channels, gateway, tui        │
│  (depends on Core + Foundation)                      │
├─────────────────────────────────────────────────────┤
│                      Core Layer                      │
│  provider, tools, tools-macros, runtime, memory,     │
│  sandbox                                             │
│  (depends strictly on Foundation; intentional         │
│   sub-layering: runtime → tools → sandbox)           │
│  (dev-dependencies may cross layer boundaries        │
│   for integration testing, e.g. shell-daemon)        │
├─────────────────────────────────────────────────────┤
│                   Foundation Layer                   │
│  types                                               │
│  (zero internal dependencies)                        │
└─────────────────────────────────────────────────────┘
```

### What Lives Where

| Layer | Crate | Responsibility |
|-------|-------|---------------|
| Foundation | `types` | Universal structs (`Message`, `ToolCall`, `Context`, `Response`), all trait definitions (`Provider`, `Tool`, `Memory`, `Channel`), error hierarchy, config schemas, runner protocol types |
| Core | `provider` | Provider implementations (OpenAI, Anthropic, Gemini, OpenAI Responses), SSE parsing, reliability wrapper, credential resolution |
| Core | `tools` | Core tool implementations (file ops, shell, web, vault), tool registry with policy enforcement |
| Core | `tools-macros` | `#[tool]` attribute macro for automatic schema generation from Rust function signatures |
| Core | `runtime` | Agent turn loop, state machine, tool dispatch, self-correction, token budgeting, credential scrubbing |
| Core | `memory` | libSQL persistence, SQL migrations, hybrid retrieval (vector + FTS5), embedding pipeline, rolling summarization |
| Core | `sandbox` | WASM tool isolation, security policy enforcement, shell/browser session management, SSRF protection |
| Application | `runner` | Host entry point, per-user VM/container provisioning, bootstrap envelope, workspace directory creation, VM bootstrap logic (config loading via `figment`, provider/memory/tools initialization) |
| Application | `shell-daemon` | Guest-side RPC server for shell command execution and browser session management |
| Application | `channels` | Channel registry, concrete channel adapter implementations (feature-flagged) |
| Application | `gateway` | Axum WebSocket server, session management, turn routing to `AgentRuntime` |
| Application | `tui` | `TuiChannelAdapter` (Channel impl), gateway WebSocket client, terminal UI rendering |

## Key Design Decisions

### Dynamic Dispatch for Extensibility

The core traits (`Provider`, `Tool`, `Memory`, `Channel`) use dynamic dispatch via `Box<dyn Trait + Send + Sync>`. This enables heterogeneous collections (e.g., a registry holding multiple provider types simultaneously) at the cost of a vtable indirection per call. Since async functions in traits are not `dyn`-compatible on stable Rust, all async trait boundaries use `#[async_trait]` for future erasure.

### Typed Identifiers Over Strings

Model and provider selection uses newtype wrappers (`ModelId(String)`, `ProviderId(String)`) rather than raw strings. Combined with the `ModelCatalog` (a build-time-embedded JSON snapshot of supported models), this catches invalid model references at startup rather than at inference time.

### Embedded Everything

The system embeds its database (libSQL), embedding model (fastembed-rs or blake3 fallback), and model catalog (pinned JSON snapshot) directly into the binary. No external services are required for local operation. Remote connectivity (Turso for shared databases, API-based embeddings) is available via configuration but never required.

### Compiler-Enforced Boundaries

The `deny.toml` configuration and CI quality checks (`cargo deny`, `clippy`, `rustfmt`) enforce:
- License compliance (allowlist of OSI-approved licenses)
- Supply chain security (no unknown registries or git sources)
- No duplicate crate versions (warned)
- Zero clippy warnings (denied)

## Architecture Diagram

```
                         ┌──────────────┐
                         │   Runner     │
                         │ (host entry) │
                         └──────┬───────┘
                                │ bootstrap envelope
                    ┌───────────┴───────────┐
                    │                       │
              ┌─────▼──────┐        ┌───────▼───────┐
              │  oxydra-vm │        │   shell-vm    │
              │            │        │               │
              │ ┌────────┐ │  vsock │ ┌───────────┐ │
              │ │Gateway │◄├────────┤►│Shell Daemon│ │
              │ │Server  │ │        │ │           │ │
              │ └────┬───┘ │        │ └───────────┘ │
              │      │     │        └───────────────┘
              │ ┌────▼───┐ │
              │ │Runtime │ │
              │ │  Loop  │ │
              │ └─┬──┬───┘ │
              │   │  │     │
              │ ┌─▼┐┌▼──┐  │
              │ │  ││Mem │  │
              │ │T ││ory │  │
              │ │o ││    │  │
              │ │o │└────┘  │
              │ │l │        │
              │ │s │        │
              │ └──┘        │
              └─────────────┘
                    ▲
                    │ WebSocket
              ┌─────┴──────┐
              │    TUI     │
              │ (terminal) │
              └────────────┘
```

## Error Philosophy

Every crate defines its own `thiserror` error enum. Errors compose upward:
- `ProviderError`, `ToolError`, `MemoryError` are domain-specific
- `RuntimeError` wraps all domain errors plus `Cancelled` and `BudgetExceeded`
- `ChannelError` and `ConfigError` are independent error domains
- `RunnerConfigError` handles runner-specific validation failures

Errors are categorized as **recoverable** (tool failures become self-correction prompts, stream drops trigger retries) or **fatal** (budget exceeded, cancelled, unauthorized). The runtime never panics on LLM-originated errors; they are always formatted and fed back for model self-correction.
