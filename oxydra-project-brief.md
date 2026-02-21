# Architecting a High-Performance, Minimalist AI Agent Orchestrator in Rust

**A Progressive Systems Engineering Guide**

---

## Table of Contents

1. Introduction
2. Chapter 1: The Minimal Core Philosophy and Progressive Monorepo Architecture
3. Chapter 2: The Unified Provider Abstraction and Trait Dispatch
4. Chapter 3: The Agent Runtime and Turn-Based State Machine
5. Chapter 4: Asynchronous Streaming and Input Output Mechanics
6. Chapter 5: Tool Calling, Procedural Macros, and Schema Validation
7. Chapter 6: Conversational Memory and Vector State Persistence
8. Chapter 7: Capability-Based Security and OS-Level Sandboxing
9. Chapter 8: Multi-Agent Orchestration, Routing, and The Gateway Pattern
10. Chapter 9: Productization Backlog Integration After Core Runtime
11. Conclusion

---

## Introduction

The transition from monolithic, script-based artificial intelligence applications to highly scalable, autonomous agent orchestrators represents a fundamental paradigm shift in software architecture. Early implementations of agentic systems relied heavily on dynamic languages, primarily Python and TypeScript, prioritizing rapid prototyping and ecosystem availability over execution efficiency. Frameworks such as OpenClaw demonstrated the immense potential of personal AI assistants, moving beyond simple chatbots to entities capable of persistent memory, multi-channel presence, proactive background task execution, and autonomous software development.[1, 2] OpenClaw transformed agentic capabilities from research artifacts into deployable digital teammates, characterized by users as an "iPhone moment" for its ability to automate tasks across inboxes, calendars, and integrated development environments.[2, 3]

However, as these systems scale to handle multimodal streams, complex multi-agent routing, and concurrent tool execution, the overhead of heavily abstracted runtime environments becomes a critical architectural bottleneck. In an ecosystem where a single TypeScript-based agent runtime can consume over 1 gigabyte of random access memory and require upwards of 500 seconds for complex cold starts, transitioning to a systems-level language becomes an operational imperative.[4] Constructing a highly concurrent, memory-safe, and dynamically extensible agent orchestrator in Rust offers profound advantages in both cost and performance. Systems like ZeroClaw have proven that an entire AI agent infrastructure can be condensed into a 3.4-megabyte compiled binary capable of executing with less than 5 megabytes of memory overhead and a cold start latency of under 10 milliseconds.[5, 6] This lean architecture enables deployment on hardware as constrained as a $10 single-board computer, making it 98 percent cheaper to operate than traditional heavy runtimes.[4, 6]

Achieving this requires a rigorous, progressive architectural strategy. Rather than attempting to construct a monolithic framework in a single iteration—a process prone to architectural drift and unmaintainable coupling—the system must be built layer by layer. This approach adheres strictly to the "small core" philosophy demonstrated by architectures like the Pi-mono monorepo, where the intentional omission of bloated features forces the agent to rely on a minimal set of highly optimized, generalized capabilities to extend itself dynamically.[2, 7] By isolating the foundational language model abstractions, constructing a minimal agent loop, engineering runner-enforced sandboxing (microVM/container + WASM), and finally multiplexing through a centralized gateway, one can develop an orchestrator that is secure by design, infinitely extensible, and capable of deep multi-agent collaboration without succumbing to technical debt.

---

## Chapter 1: The Minimal Core Philosophy and Progressive Monorepo Architecture

The foundational philosophy of a robust, production-grade agent orchestrator is rooted in intentional reduction. The Pi agent architecture avoids hardcoding expansive toolsets and protocol-specific integrations into the core runtime, including standards such as MCP.[2] This omission is not developmental oversight; it is a deliberate design choice to keep the core minimal and extensible. The philosophy dictates that if an agent requires new capabilities or integrations, the user should not simply download a pre-compiled extension or skill.[2] Instead, the user should instruct the agent to extend itself, leveraging the innate code-generation proficiencies of large language models to write, execute, and persist new capabilities at runtime.[2]

To successfully implement this philosophy in a Rust environment, the architecture must be progressively constructed using a strict, enforced dependency hierarchy. Utilizing Cargo workspaces, the repository should adopt a layered structure inspired by Pi-mono's conceptual dependency boundaries while remaining idiomatic to Rust crate design.[7, 8] This monorepo design ensures that the lowest foundational packages contain zero internal dependencies, the core logic packages depend exclusively on the foundations, and the high-level orchestration routing applications depend only on the core.[7]

| Architectural Layer | Crate / Package | Scope | Dependency Constraint | Core Responsibility |
|---|---|---|---|---|
| Foundation | `types` | Zero internal dependencies | Defines universal message/tool/context structs, base traits, config schema structs, and shared error primitives. |
| Core | `provider`, `tools`, `tools-macros`, `runtime`, `memory`, `sandbox` | Depends strictly on Foundation | Implements provider adapters, tool execution, turn loop, persistence, and sandbox boundaries. |
| Application | `runner`, `shell-daemon`, `channels`, `gateway`, `tui` | Depends on Core and Foundation (`tui` depends on `types` + `channels`) | Manages host-side sandbox orchestration, guest-side shell/browser daemon services, ingress/egress channels, routing daemon, and operator-facing terminal channel UX. |

By enforcing these boundaries at the compiler level, the orchestrator's core runtime remains entirely decoupled from specific communication channels, terminal user interfaces, or external application programming interfaces. This separation allows a developer to focus on the stability of the abstraction layer before attempting to route multi-agent conversations. Furthermore, it supports the Pi philosophy of providing the shortest viable system prompt alongside a highly restricted toolset—specifically Read, Write, Edit, and Bash.[2, 7] By providing only these four primitives, the agent is granted full Turing-complete control over its environment, relying on its internal understanding of command-line tools to navigate the host system rather than requiring the developer to hardcode specialized Rust traits for every conceivable operation.[7]

### Entities to Implement

- **Cargo Workspace Root:** Define `[workspace]` in the root `Cargo.toml` and declare all crates explicitly.
- **Configuration Module (`types::config`):** Define typed config structs and defaults shared across runtime, provider, and gateway crates.
- **Error Hierarchy:** Define per-crate `thiserror` enums and compose them upward (`RuntimeError` wrapping provider/tool/memory failures).
- **Layered Crates:** Initialize `types`, `provider`, `tools`, `runtime`, `memory`, `sandbox`, `runner`, `shell-daemon`, `channels`, `gateway`, and `tui` from day one to avoid structural rewrites later.

### Architecture Diagram So Far

```mermaid
graph TD
App --> Core
Core --> Found
style Found fill:#d4edda,stroke:#28a745,stroke-width:2px
style Core fill:#d4edda,stroke:#28a745,stroke-width:2px
style App fill:#d4edda,stroke:#28a745,stroke-width:2px
```

### Implementation Hints

- Enforce the dependency graph strictly during your build process. If your foundation layer accidentally imports tokio filesystem utilities meant for the application layer, you have broken the separation of concerns.
- Treat pi-mono as a conceptual reference (layering, minimal core), but implement the design natively with Cargo workspaces.[7]
- Add `cargo deny` and feature-gated builds in CI to keep optional capabilities (`gateway`, channel adapters, MCP, OpenTelemetry) out of early phases.

### Configuration Management Strategy (Foundational)

Configuration must be deterministic and auditable from the first milestone. Use one layered loader (`config` or `figment`) with a single precedence order:

1. Built-in defaults in code
2. System config (`/etc/oxydra/agent.toml`, `/etc/oxydra/providers.toml`)
3. User config (`~/.config/oxydra/agent.toml`, `~/.config/oxydra/providers.toml`)
4. Workspace overrides (`./.oxydra/agent.toml`, `./.oxydra/providers.toml`)
5. Environment overrides (for example `OXYDRA__RUNTIME__MAX_TURNS`)
6. CLI flags (highest precedence)

Each file should include a `config_version` field and must be validated at startup (typed deserialization + schema checks). Startup should fail fast on invalid configuration to avoid undefined runtime behavior. Credentials should resolve consistently as: explicit config `api_key` -> provider-specific environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`) -> generic `API_KEY` fallback (development only). Secrets must never be printed in logs or trace spans.

### Finalized Design Decisions

- Pi-mono is used as a conceptual reference only; this proposal is explicitly a Rust/Cargo architecture.
- Workspace boundaries are enforced with CI policy (`cargo deny`) and capability feature flags to keep early phases minimal.
- Configuration and error hierarchy are now foundational concerns, not deferred concerns.

### Considerations

- Standardize on either `config` or `figment` by implementation kickoff to avoid divergent precedence behavior.

---

## Chapter 2: The Unified Provider Abstraction and Trait Dispatch

The initial phase of development must focus entirely on the communication boundary between the orchestrator and the external AI models. A naive approach involves writing hardcoded HTTP requests for specific inference APIs, which inherently locks the system into a single provider and severely limits operational flexibility. Instead, the foundation must establish a unified multi-provider abstraction layer, akin to the pi-ai implementation, which normalizes disparate wire protocols into a singular, cohesive internal representation.[7, 8]

### Normalizing Wire Protocols

Despite the proliferation of over 22 distinct AI providers—including OpenAI, Anthropic, Google Gemini, Ollama, and DeepSeek—the underlying data exchange generally adheres to a limited set of conceptual wire protocols.[4, 5] By isolating these protocols, the Rust implementation can map any external API to a universal, portable schema. The abstraction layer is responsible for converting the orchestrator's internal `Message` and `ToolCall` structs into the specific JSON payloads expected by the provider.[9]

#### Type-First Model Identity and Capability Registry

Model and provider selection should be type-first, not string-first. Avoid carrying model names as free-form `String` values across runtime boundaries; instead define strongly typed identifiers such as `ModelId`, `ProviderId`, and `ModelDescriptor`. A generated model catalog (for example from a pinned models.dev snapshot, as done conceptually in pi-mono) should be used to populate capability and policy metadata (`supports_tools`, `supports_streaming`, token limits, pricing, deprecation state). Runtime and configuration code should validate model selection against this catalog at startup so unknown or malformed model identifiers fail fast before execution.

For instance, OpenAI and Ollama utilize the standard `/v1/chat/completions` endpoint, which translates natively to internal representations.[7] In contrast, Anthropic's Claude relies on a `/v1/messages` block structure that interleaves text and tool-use blocks, requiring the abstraction layer to flatten these elements into standardized turns.[7] Google's Generative AI operates on a proprietary `parts` array within its `/v1beta/models/{model}:generateContent` endpoint, demanding custom structural translation.[7] Furthermore, the abstraction layer must internally manage the specific idiosyncrasies of each provider, such as mapping variations in token limitation parameters, managing the absence of native tool-call streaming in specific endpoints, and normalizing "thinking" or reasoning traces across different inference engines.[7] The output of this layer is a catalog of context objects that are entirely serializable, allowing the orchestrator to dynamically switch providers mid-session while preserving the conversation history and cognitive trace.[7, 10]

#### Supporting the OpenAI Responses API Format

Beyond the standard `/v1/chat/completions` endpoint, the abstraction layer must also account for OpenAI's **Responses API** (`/v1/responses`), a newer stateful inference format introduced to support richer agentic workflows. Unlike the chat completions endpoint—which is stateless and requires the client to transmit the full conversational history on every request—the Responses API persists conversation state server-side via a `previous_response_id` chaining mechanism. This fundamentally alters how the provider implementation manages context.

The Responses API introduces several structural differences that the provider abstraction must normalize:

- **Stateful session chaining:** Rather than re-sending the full `messages` array, each subsequent turn references the prior turn's `response.id`. The `ResponsesProvider` implementation must track this identifier in session state and inject it as `previous_response_id` in the request body.
- **Input array format:** The Responses API accepts an `input` array of typed content items (e.g., `input_text`, `input_image`) rather than a flat `messages` array, requiring a dedicated serialization path in the provider's request builder.
- **Built-in tool declarations:** The Responses API supports first-class built-in tools such as `web_search` and `file_search` declared directly in the request payload, distinct from the `functions` schema used by chat completions. The abstraction layer must map the orchestrator's internal `ToolDecl` structs to the appropriate format depending on the active endpoint.
- **Output item parsing:** Responses are returned as a structured `output` array of typed items (e.g., `output_text`, `tool_call`, `reasoning`), rather than a flat `choices[0].message` structure. The provider implementation must iterate these items and flatten them into the orchestrator's internal `Response` representation.
- **Streaming via SSE:** Like chat completions, the Responses API supports streaming via Server-Sent Events, but emits distinct event types such as `response.output_item.added` and `response.output_text.delta`, requiring a separate SSE parser branch in the async streaming layer covered in Chapter 4.

When implementing the `ResponsesProvider` struct, the provider's `complete` method should detect whether a live `previous_response_id` exists in the current session context. If one is present, it omits the full message history and instead chains the response, drastically reducing token transmission overhead for long multi-turn sessions. This server-side state management capability makes the Responses API particularly well-suited for multi-agent subagent delegation patterns described in Chapter 8, where spawning a new subagent with a fresh `response_id` chain provides clean context isolation without duplicating memory payloads.

### Static versus Dynamic Dispatch in Trait Architecture

In Rust, exposing the extension surface for providers, channels, and tools requires a sophisticated architectural approach to trait implementations. The orchestrator must define a `Provider` trait that standardizes inference execution, alongside corresponding traits for `Channel`, `Tool`, and `Memory` subsystems.[5] When designing these traits, the systems architect must evaluate the trade-offs between static dispatch and dynamic dispatch.[11]

Static dispatch utilizes generic boundaries, requiring the compiler to duplicate the function for every distinct type passed to it and resolving the exact memory address of the function at compile time.[11] This zero-cost abstraction eliminates runtime overhead and enables aggressive compiler optimizations, such as inlining.[11, 12] However, an agent orchestrator fundamentally requires heterogeneous collections. A gateway router may need to maintain an active registry of multiple subagents, each utilizing a different provider model simultaneously to optimize costs. In a strictly statically dispatched system, attempting to store these models in a vector mandates that the generic type resolves to a single underlying provider at compile time, preventing the mixing of an OpenAI provider and a local Llama provider within the same active registry.[11]

Consequently, the core architecture must leverage dynamic dispatch via trait objects, utilizing syntax such as `Box<dyn Provider + Send + Sync>`. Dynamic dispatch introduces a "fat pointer" comprising a pointer to data and a pointer to the virtual method table for that specific type.[11] At runtime, the system consults the vtable to locate the correct function address before executing the jump instruction.[11] While this incurs a marginal cost, it is the mechanism that enables plug-and-play provider registries and live provider switching.[5, 11] On stable Rust, `async fn` in traits is still not `dyn`-compatible for this use case, so the production-safe pattern is to define async trait boundaries with `#[async_trait]` (or an equivalent erasure crate) and keep strict `Send`/`Sync` bounds for multi-threaded runtimes.

### Entities to Implement

- **`Message` and `ToolCall` Structs:** Universal internal representations.
- **`Provider` Trait:** Async trait boundary via `#[async_trait]` with `complete`, `stream`, and `capabilities` methods returning `ProviderError`.
- **`ProviderCaps` Struct:** Declares feature support (`supports_streaming`, `supports_tools`, `supports_json_mode`, `supports_reasoning_traces`, token limits, and cost metadata).
- **`ModelId` / `ProviderId` Newtypes:** Typed identifiers replacing free-form string model selection in runtime interfaces.
- **`ModelCatalog` Registry:** Build-time or startup-loaded catalog of valid models and capabilities (for example from pinned models.dev data).
- **Provider Implementations:** Create `OpenAIProvider`, `AnthropicProvider`, `GeminiProvider`, `ResponsesProvider`, etc., that implement the trait.
- **`ProviderRouter` / `ReliableProvider`:** Wrapper layer for retries, exponential backoff, and fallback chains.

### Architecture Diagram So Far

```mermaid
graph TD
App --> Core
Core --> Found
subgraph Foundation
ProvTrait
OAI[OpenAI Impl]
Anth[Anthropic Impl]
ProvTrait -.-> OAI
ProvTrait -.-> Anth
end
style ProvTrait fill:#cce5ff,stroke:#004085,stroke-width:2px
style OAI fill:#fff3cd,stroke:#856404,stroke-width:2px
style Anth fill:#fff3cd,stroke:#856404,stroke-width:2px
```

### Implementation Hints

- Avoid tying internal `Message` structs strictly to one vendor; keep OpenAI, Anthropic, and Gemini wire mappings in provider-specific serializers.[7]
- Prefer typed model identifiers and validated descriptors over raw model-name strings in config, routing, and provider selection paths.
- Normalize tool declarations via a `ToolsPayload` enum so each provider receives its native format while the runtime stays provider-agnostic.
- Resolve credentials in the provider initialization path using the global precedence chain (explicit config -> provider env var -> generic fallback).
- Wrap each concrete provider in a `ReliableProvider` to handle retries, backoff, and fallback routing before surfacing failures to runtime.
- Keep `previous_response_id` in session state for Responses API chaining to reduce repeated token transmission in long sessions.
- If evaluating external crates such as `genai` or `rig-core`, keep them behind adapter structs implementing internal traits so no external types leak into runtime APIs.

### Finalized Design Decisions

- Provider traits are defined for stable Rust dynamic dispatch using `#[async_trait]` and `Box<dyn Provider + Send + Sync>`.
- `ProviderCaps` and `ToolsPayload` normalization are required to adapt behavior per vendor without leaking vendor-specific assumptions into runtime.
- Model selection is type-checked through internal model/provider identifiers and validated against a model capability catalog.
- Provider reliability (retry, backoff, fallback) is part of the provider layer contract, not an afterthought.
- Credential lookup uses deterministic 3-tier resolution in provider initialization.

### Considerations

- `dynosaur` or manual future erasure can be explored later if profiling shows `#[async_trait]` overhead matters.
- Decide update cadence and pinning policy for external model-catalog sources (for example models.dev snapshots) to balance freshness and reproducibility.

---

## Chapter 3: The Agent Runtime and Turn-Based State Machine

With the provider abstraction solidified, the second progressive layer involves constructing the central nervous system of the orchestrator: the agent loop. The pi-agent-core package exemplifies this minimalist design by avoiding complex built-in planning heuristics in favor of a straightforward, deterministic cycle.[7] The runtime logic shifts the burden of cognitive progression entirely to the language model, relying on robust state management within the Rust binary to facilitate execution.[13]

### The Minimal Turn-Based Cycle

The execution loop initiates by submitting the unified context payload to the dispatched `Provider` trait and listening for a streamed response. The cycle streams generated tokens to the client, evaluates completion payloads for tool calls, and terminates the turn when no tools are requested.[7] If tool calls are detected, the loop suspends text generation, executes side-effecting tools sequentially and safe read-only tools in parallel, appends normalized tool results to context, and recursively re-invokes the provider. Every turn runs under cancellation, timeout, and budget guards.

This turn-based mechanism is robust because it is deterministic and explicit. The runtime should still enforce non-negotiable operational guards: per-turn timeout, max-turn budget, and max-cost budget to prevent runaway loops. If the language model requires a complex planning phase, it can externalize that plan through tools without introducing hidden planner heuristics into the core executor.[7]

### Hierarchical State Management

To support more complex agentic behaviors without bloating the core loop, the orchestrator should implement a hierarchical state machine pattern, comparable to the designs found in the `ai-agents-memory` crate.[13] This involves mapping nested sub-states where each state can inherit prompts from its parent.[13] Transitioning between states relies on evaluated conditions, allowing the orchestrator to short-circuit execution based on predefined guardrails.[13] When the agent transitions from a "planning" state to an "execution" state, the Rust state machine triggers entry and exit actions—such as automatically executing a specific setup tool or refreshing the context window—ensuring that the agent operates within strictly defined behavioral corridors.[13] This declarative pipeline approach allows developers to define extraction, validation, and transformation logic independently of the raw inference loop.[13]

### Entities to Implement

- **`TurnState` Enum:** Represents the current phase of the agent (e.g., `Streaming`, `ToolExecution`, `Yielding`).
- **`AgentRuntime` Struct:** Holds `Box<dyn Provider>`, tool registry, context state, and runtime guards.
- **Runtime Guards:** `CancellationToken`, turn timeout, max-turn, and max-cost enforcement.
- **Telemetry Spans:** Structured `tracing` spans for turn lifecycle, provider calls, and tool execution.[7]

### Architecture Diagram So Far

```mermaid
graph TD
App --> Core
Core --> Found
subgraph Core
Loop[Agent Loop]
State
Loop <--> State
end
subgraph Foundation
ProvTrait
end
Loop --> |Sends Context| ProvTrait
style Loop fill:#cce5ff,stroke:#004085,stroke-width:2px
style State fill:#cce5ff,stroke:#004085,stroke-width:2px
```

### Implementation Hints

- Keep the loop deterministic; do not add heuristic "guessing" branches for completion semantics.
- Add provider and tool retry policies with bounded attempts, then escalate to human-in-the-loop when failure thresholds are exceeded.
- Instrument the loop with `#[instrument]` spans from day one so traces can be exported directly to OpenTelemetry later.
- Model sessions as trees rather than linear arrays.[2] This allows side-quests to branch off and collapse back without destroying the primary context.

### Finalized Design Decisions

- Runtime now explicitly includes cancellation, timeout, max-turn, and max-cost guards.
- Parallel tool execution is supported for safe/read-only tools; side-effecting tools remain sequential by default.
- `tracing` instrumentation is treated as core runtime infrastructure from the first implementation phase.

### Considerations

- A typestate-based loop can replace the enum-based state machine after behavioral stability if stronger compile-time guarantees are needed.

---

## Chapter 4: Asynchronous Streaming and Input Output Mechanics

Handling streaming responses from language models in Rust introduces significant complexities regarding asynchronous input/output management. The response from the provider must be treated as a continuous flow of data chunks transmitted via Server-Sent Events.[14] Rust's asynchronous ecosystem, while immensely powerful, presents competing paradigms for stream handling, demanding careful architectural selection.

### Navigating the Stream and AsyncRead Traits

The fundamental choice in handling incoming tokens lies between the `Stream` trait and the `AsyncRead` trait.[15] The `Stream` trait defines a low-level interface that effectively combines the mechanisms of iterators and futures, allowing developers to utilize higher-level utilities provided by `StreamExt`, such as the asynchronous `.next()` method.[34] Conversely, the `AsyncRead` trait mirrors traditional synchronous reading by operating on byte buffers, with variations existing between the standard futures implementation and the tokio runtime implementation.[15]

In a high-throughput orchestrator designed to manage concurrent multi-agent communications, blocking operations are catastrophic to the event loop. The implementation must utilize asynchronous message passing, typically via `tokio::sync::mpsc` channels.[16] The architecture spawns an isolated asynchronous task that maintains the HTTP connection with the provider, parsing the incoming SSE payload as the network delivers chunks.[14] Each parsed delta—whether a fragment of natural language or a partial JSON tool-call argument—is transmitted through the sender channel. The primary agent loop acts as the receiver, yielding these chunks to the connected user interfaces or accumulating them into an internal buffer to reconstruct complete tool arguments before execution.[16]

### Waker Mechanics and Dynamic Dispatch Constraints

A critical, low-level nuance of Rust's asynchronous stream polling is the management of the `Waker` mechanism. The `Future` poll cycle uses a `Waker` to resume suspended tasks once network or parser state changes.[17] For practical orchestrator architecture, this reinforces two implementation choices: keep provider streaming adapters object-safe at runtime boundaries, and isolate stream parsing in dedicated tasks communicating over bounded channels. This avoids backpressure collapse while preserving modular provider implementations.

### Entities to Implement

- **Bounded `tokio::sync::mpsc::Sender` / `Receiver`:** Use bounded channels to pass parsed stream chunks with backpressure.
- **`StreamItem` Enum:** Include `Text`, `ToolCallDelta`, `ReasoningDelta`, `UsageUpdate`, `ConnectionLost`, and `FinishReason`.
- **Tool Chunk Accumulator:** Reconstruct fragmented JSON arguments safely before validation, including escaped sequences and split unicode fragments.[35]

### Architecture Diagram So Far

```mermaid
graph TD
App --> Core
subgraph Core
Loop[Agent Loop]
Buffer
end
subgraph Async IO Thread
SSE
end
SSE -- mpsc channel --> Loop
Loop --> Buffer
style SSE fill:#cce5ff,stroke:#004085,stroke-width:2px
style Buffer fill:#cce5ff,stroke:#004085,stroke-width:2px
```

### Implementation Hints

- Use bounded `mpsc::channel(N)` to apply natural backpressure when downstream consumers are slower than provider streams.
- Implement reconnect/resume behavior for providers that support resumable streams; otherwise emit `ConnectionLost` so runtime can retry the turn.
- Tool chunks arrive heavily fragmented. The accumulator must safely handle partial escapes, partial unicode, and concurrent multi-tool deltas before validation.
- Emit real-time usage deltas during streaming to power cost and budget enforcement.

### Finalized Design Decisions

- Streaming is implemented with bounded channels to enforce backpressure under slow consumers.
- Connection resilience is explicit: reconnect/resume where supported, otherwise emit structured loss events for runtime policy.
- Stream parsing includes reasoning and usage deltas to support observability and budget enforcement.

### Considerations

- Choose between standard `serde_json` incremental parsing and SIMD-accelerated parsers based on measured throughput requirements.

---

## Chapter 5: Tool Calling, Procedural Macros, and Schema Validation

An autonomous agent is only as effective as the environment it can manipulate. The fourth progressive layer establishes the `Tool` trait, enabling the orchestrator to execute concrete programmatic actions.[5] Implementing tool calling securely and reliably requires bridging the strongly typed, compiled domain of Rust with the inherently loosely structured JSON generation of probabilistic language models.

### Automatic JSON Schema Generation

To inform the language model of its capabilities, the orchestrator must supply strictly formatted JSON schemas detailing the available tool names, descriptions, and required arguments.[9] Manually defining and maintaining these JSON structures alongside the corresponding Rust functions is highly prone to architectural drift, where the documentation diverges from the actual executable code.

The optimal architectural pattern employs Rust procedural macros to automate this synchronization, leveraging concepts demonstrated by crates such as `tools-rs`.[19] By applying a custom attribute macro (e.g., `#[tool]`) to an asynchronous Rust function, the compiler can automatically derive the necessary metadata during the build process.[18, 19] This involves parsing the function signature, mapping Rust primitives and custom structs to their JSON Schema equivalents, and extracting doc-comments to formulate the tool's descriptive payload.[19] This ensures that the generated `FunctionDecl` schema supplied to the model is in perfect, type-safe synchronization with the binary's actual capabilities.[19]

### Validation and Self-Correction Loops

Despite explicit schema declarations, language models frequently generate non-compliant JSON payloads. Errors manifest as missing required fields, hallucinated parameters, incorrect data types, or failure to escape internal quotation marks.[20] If the orchestrator naively attempts to deserialize this malformed output into a Rust struct, the operation will panic or return a fatal error, terminating the agent's execution loop.

To mitigate this systemic fragility, the orchestrator must implement an aggressive in-process validation layer using Rust-native tooling (`schemars`/`tools-rs` for schema generation, `jsonschema` + `serde_json` for validation and decoding).[20] When the agent yields a tool call, arguments are buffered and validated against the expected Rust type signature.[19] If validation fails, the orchestrator does not crash. Instead, it captures the specific decoding/validation error, formats it as a structured tool-result error, and injects it back into conversation context for model self-correction.[7] This pattern transforms a fatal runtime failure into a recoverable iteration step, significantly improving reliability in long multi-step workflows.

### Entities to Implement

- **`dyn Tool` Trait:** Requires `fn schema(&self) -> FunctionDecl`, `async fn execute(&self, args: &str) -> Result<String, ToolError>`, `fn timeout(&self) -> Duration`, and `fn safety_tier(&self) -> SafetyTier`.
- **`#[tool]` Procedural Macro:** Macro crate to auto-generate function schemas from Rust signatures and docs.[19]
- **Validation Guard:** Uses `jsonschema` + `serde_json` to catch malformed arguments and return structured correction payloads.[20]
- **Tool Registry Policy Layer:** Enforces timeout, output-size truncation, and HITL gates by safety tier before results are appended to context.

### Architecture Diagram So Far

```mermaid
graph TD
subgraph Core
Loop[Agent Loop]
Guard
end
subgraph Tools Layer
ToolTrait
FileRead
Bash
end
Loop --> |Yields Arguments| Guard
Guard --> |Validates & Deserializes| ToolTrait
Guard -.-> |Syntax Error| Loop
ToolTrait -.-> FileRead
ToolTrait -.-> Bash
style ToolTrait fill:#cce5ff,stroke:#004085,stroke-width:2px
style Guard fill:#f8d7da,stroke:#721c24,stroke-width:2px
```

### Implementation Hints

- Keep baseline tools minimal: `Read`, `Write`, `Edit`, and `Bash` are sufficient for full environmental manipulation.[2]
- Enforce a mandatory timeout at registry level for every tool invocation.
- Truncate oversized tool output before context injection and include a clear truncation notice with original byte count.
- If an LLM yields a hallucinated parameter, include the violated schema fragment and decoder error to maximize self-correction quality.
- Keep the `Tool` trait MCP-compatible so `McpToolAdapter` can be added without changing native tool signatures.

### Finalized Design Decisions

- Tool validation uses Rust-native schema tooling (`schemars`/`tools-rs`, `jsonschema`, `serde_json`) in-process.
- Registry-level policy enforces timeout, output-size limits, and safety-tier gating uniformly across all tools.
- Tool trait design remains MCP-compatible to support future external tool registries without redesign.

### Considerations

- Keep procedural macros focused on schema generation; avoid embedding runtime policy logic in macro expansions.

---

## Chapter 6: Conversational Memory and Vector State Persistence

An orchestrator managing continuous, multi-turn, multi-agent workflows requires an unyielding state management system. In-memory data structures are entirely insufficient for long-running daemon processes that may be interrupted, restarted, or required to context-switch between hundreds of concurrent user sessions. The `Memory` trait must persist conversational state, tool results, and semantic context reliably across the host machine's filesystem.[5]

### Embedded libSQL and Hybrid Retrieval

While heavy standalone vector databases—such as Pinecone, Qdrant, or Weaviate—are prevalent in enterprise architectures, they require complex containerization, networking overhead, and HTTP serialization that severely violate the orchestrator's constraints of being local-first and minimally resourced.[21] The architectural solution is to embed a two-tier database directly into the Rust binary, leveraging libSQL (SQLite-compatible, Turso-backed) in local mode with optional remote connectivity for shared deployments.[22, 23]

The schema design for this highly optimized memory architecture necessitates distinct, synchronized tables to support hybrid retrieval techniques.[22]

- A `files` table tracks the physical location, content hashes, and modification times of ingested context documents to prevent redundant indexing.[22]
- A primary `chunks` table serves as the source of truth, storing the raw text data, JSON-serialized metadata, and exact line ranges for localized extraction.[22]
- A `chunks_vec` table stores binary float vectors generated by a local embedding model, using libSQL/Turso vector capabilities to enable rapid nearest-neighbor similarity searches without special extension loading.[22]
- A `chunks_fts` virtual table, utilizing libSQL's SQLite-compatible FTS5 support, indexes the raw text to provide exact keyword searches and token matching.[22]

This structure empowers the agent with true hybrid retrieval capabilities. When an agent queries long-term memory, the orchestrator executes cosine similarity search on vectors and BM25 scoring on FTS5 in parallel, then merges results with a weighted ranker.[4, 22] For production readiness, embedded libSQL should remain the default path because it preserves local-first portability while aligning with Turso-hosted deployments. Optional Turso remote mode can be enabled by configuration when replication/shared access is required, without changing the `Memory` trait contract.

### Rolling Summarization and Context Constraints

Continuous conversational persistence inevitably encounters the maximum context window limitation of the underlying language model. To maintain state indefinitely without triggering context collapse, the Memory subsystem must implement rolling summarization and token budgeting strategies.[13] As the session size crosses a predefined token ratio threshold—for example, exceeding 85 percent of the model's maximum capacity—the system must proactively trigger a background compression task.[24]

An asynchronous inference call is dispatched using a specialized summarizer prompt. It digests older conversational nodes, extracts key facts, and updates synthesized long-term memory while truncating raw logs.[13, 24] The summarization trigger threshold should be configurable per model (not hardcoded), and updates must be race-safe (for example via context epoch checks or lock-guarded writes) so a newer turn is never overwritten by stale summarizer output.[25] Structuring sessions as hierarchical trees further allows side-quests to branch and merge cleanly without polluting primary context windows.[2]

### Entities to Implement

- **`Memory` Trait:** Abstraction containing `store`, `recall`, and `forget` methods.
- **Embedded libSQL DB:** `libsql` + versioned SQL migrations (for example `refinery`) for durable schema evolution.
- **Embedding Pipeline:** Local-first embedding generation (for example `fastembed-rs`) with optional API-based providers behind feature flags.
- **Context Budget Manager:** Tracks model-specific token budgets (`system + memory + history + tools + safety buffer`) and triggers summarization by configurable thresholds.[24]
- **Background Summarizer:** Async task with race protection (epoch or lock strategy).

### Architecture Diagram So Far

```mermaid
graph TD
subgraph Core
Loop[Agent Loop]
MemTrait
end
subgraph Storage Layer
SQL
Vec
FTS
end
Loop <--> MemTrait
MemTrait --> SQL
SQL --> Vec
SQL --> FTS
style MemTrait fill:#cce5ff,stroke:#004085,stroke-width:2px
style SQL fill:#e2e3e5,stroke:#383d41,stroke-width:2px
```

### Implementation Hints

- Instead of a separate vector database server, use libSQL/Turso native vector capabilities and keep remote Turso connectivity optional to preserve the zero-overhead local-first philosophy.[22]
- Always use hybrid retrieval (cosine + BM25 FTS5) to balance semantic and exact-match recall.[22]
- Define explicit retention policy (what is persisted, retention duration, redaction rules for PII/credentials) before enabling persistent memory in production.
- Keep API-based embedding providers optional; local embeddings should be the default for offline reliability and predictable cost.

### Finalized Design Decisions

- Embedded libSQL (Turso-compatible) remains the default memory architecture; optional Turso remote mode is enabled by configuration for shared deployments.
- Token budgeting, summarization triggers, and race-safe summarizer updates are mandatory for long-running sessions.
- Memory persistence requires retention/redaction policy from the first production deployment.

### Considerations

- API-hosted embeddings can be added for quality-sensitive workloads, but local embeddings should remain the baseline for predictable cost and offline support.

---

## Chapter 7: Capability-Based Security and OS-Level Sandboxing

The integration of unrestricted shell execution, filesystem manipulation, and dynamic tool calling introduces massive, systemic security vulnerabilities. Providing an autonomous AI agent with programmatic control over a host machine exposes the system to sophisticated prompt injections, hallucinated destructive commands, and malicious external payloads downloaded inadvertently via web searches. To build a secure-by-default orchestrator, robust security boundaries must be enforced at the operating system level, entirely bypassing the reliance on the agent's semantic understanding.[5, 26]

### The Threat Model of Autonomous Execution

Traditional cybersecurity paradigms operate on the assumption that malicious code is introduced externally. In agentic AI, the threat frequently originates from the authorized user's prompts or from poisoned data retrieved legitimately by the agent—an attack vector known as Indirect Prompt Injection.[27] A malicious skill or an injected payload can easily instruct an unsuspecting agent to exfiltrate SSH keys, AWS credentials, or browser cookies.[29] The orchestrator cannot rely on the language model's internal alignment or highly engineered system prompts to refuse these commands, as attackers continually discover novel jailbreak methodologies to bypass semantic filters.[26, 27] A deterministic, system-level guardrail is therefore mandatory.[27]

### Runner-Isolated Execution and Capability Sets

The `SecurityPolicy` trait must govern all external interactions and state modifications.[6] Rather than attempting to block specific destructive commands via a fragile denylist, the orchestrator employs a strict default-deny architecture.[28]

Host-side isolation is delegated to `oxydra-runner`, which is the system entry point and has one responsibility: spawn and wire isolated execution environments, then stand aside. The runner does not execute user code, does not access user data contents, and does not participate in agent turns.

For each user, runner provisions one isolated pair: an `oxydra-vm` guest (main Oxydra Rust process) and a `shell-vm` guest (shell daemon + browser pool). Users never share these guests. After spawn, runner sends bootstrap metadata to `oxydra-vm` using a length-prefixed JSON envelope over the startup socket; this envelope carries the `shell-vm` socket address and related startup fields, and this channel is not transported through environment variables.

If `shell-vm` startup fails or its socket is absent, `oxydra-vm` detects this during startup and marks shell/browser tools disabled with explicit status instead of silently degrading behavior. Each user receives a segregated workspace namespace (`shared`, `tmp`, `vault`) rooted at `<workspace_root>/<user_id>/`; this directory is materialized by runner on first spawn.

Runner configuration is two-level. Global operator config contains only operator-scoped infrastructure inputs: workspace root, known users (`user_id` + per-user config path), default `SandboxTier` (`MicroVm`, `Container`, `Process`), and image paths/tags for `oxydra-vm` and `shell-vm` (kept global because these must stay infrastructure-uniform and cannot be meaningfully delegated per user). Per-user config then carries user-scoped values such as mount paths, resource limits, credential references, and behavioral overrides.

`SandboxTier` is a typed enum in `types` and must be emitted in runner startup logs and exposed in health/status surfaces. In `MicroVm` and `Container` tiers, VM/container boundaries plus `wasmtime` capabilities are the primary controls; Landlock/Seatbelt are intentionally not added there. In `Process` tier (enabled via `oxydra-runner --insecure` for development/testing), runner starts Oxydra as a host process, skips `shell-vm`, auto-disables shell/browser tools, and emits a prominent warning that isolation is degraded and not production-safe. Process tier applies best-effort Landlock (Linux) or Seatbelt (macOS) filesystem restrictions strictly as harm reduction, not as a security guarantee equivalent to VM/container isolation.

### Secrets Management, Egress Filtering, and Safe Execution

To utilize API keys without exposing them to the agent's memory or bash environment, the orchestrator must integrate securely with the host's native secrets manager.[28] Credentials are loaded dynamically at runtime and mapped directly to specific outgoing HTTP headers within the Rust networking layer.[28] The agent can trigger the API call by referencing the integration, but it remains structurally incapable of dumping the environment variables to read the raw cryptographic keys.

Network egress is similarly constrained to prevent data exfiltration. `seccomp-bpf` and host firewall rules solve different layers: syscall filtering can block broad socket behavior, while domain-level allowlists require proxy- or namespace-based routing controls. For strict outbound policies, route tool traffic through a controlled proxy and allow only approved destinations (for example `api.github.com`) so policy can be enforced and audited consistently.[29, 28]

Lightweight tools execute through `wasmtime` with strict capability sets. `file_read`/`file_search`/`file_list` mount `shared`+`tmp`+`vault` read-only, `file_write`/`file_edit`/`file_delete` mount `shared`+`tmp` read-write, and `web_fetch`/`web_search` run with no filesystem mounts and no raw socket capability. Web network access is brokered only by a host-function HTTP proxy that resolves hostnames before blocklist checks and rejects loopback (`127.0.0.0/8`), link-local (`169.254.0.0/16`), RFC1918 ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), and metadata endpoints (`169.254.169.254`).

Vault copy flows are host-orchestrated as two sequential WASM invocations: one invocation mounts vault read-only for read/export, then a second invocation mounts `shared`/`tmp` for write/import. No single WASM instance receives concurrent `vault` and `shared`/`tmp` mounts. Heavyweight shell/browser tools route through shell daemon sessions in `shell-vm`; if this channel is unavailable, those tools remain disabled.

When interacting with databases, the orchestrator must constrain the agent's expressible intent. Passing a raw `execute_sql` tool to an agent invites catastrophic data loss via hallucination.[26] Instead, the architecture should employ Abstract Syntax Tree enforcement, akin to the ExoAgent methodology.[26] The agent is provided a constrained query builder capability object that compiles down to safe SQL execution, ensuring that mandatory conditional clauses and join constraints are mathematically enforced before the database engine receives the query.[26]

### Entities to Implement

- **`SandboxTier` Enum (`types`):** `MicroVm`, `Container`, `Process`; surfaced in startup logs and health/status endpoints.
- **Runner Config Types:** Global runner config (workspace root, user map, default tier, guest image references) + per-user config (mount/resource/credential/behavior overrides).
- **`RunnerControl` + Bootstrap Envelope:** Runtime-to-runner control protocol plus length-prefixed JSON startup envelope carrying sidecar socket metadata.
- **Shell Daemon Protocol:** `SpawnSession`, `ExecCommand`, `StreamOutput`, and `KillSession` operations for shell/browser workloads.
- **`ShellSession` / `BrowserSession` Traits + Backends:** `VsockShellSession` (guest channel) and `LocalProcessShellSession` (host-test/development harness) abstractions in `sandbox`.
- **`SecurityPolicy` Trait + Output Scrubber:** Command allowlist, path traversal blocking, HITL gates, credential scrubbing (`RegexSet`), and auditable policy logging.[3]

### Architecture Diagram So Far

```mermaid
graph TD
subgraph Host
Runner[oxydra-runner]
end
subgraph Per-User Pair
subgraph Oxydra VM
Loop[Oxydra Runtime]
Wasm[wasmtime Tools]
Mem[Memory Layer]
end
subgraph Shell VM
ShellDaemon[Shell Daemon + Browser Pool]
end
end
Security --> |Enforces| Runner
Runner --> |length-prefixed JSON bootstrap| Loop
Runner --> |vsock/unix socket| ShellDaemon
Loop --> Wasm
Loop --> |ShellSession/BrowserSession| ShellDaemon
Wasm -.-> |Policy Violation| KernelBlocked
ShellDaemon -.-> |Escape Attempt| KernelBlocked
style Host fill:#fff3cd,stroke:#856404,stroke-width:2px
style Security fill:#cce5ff,stroke:#004085,stroke-width:2px
style KernelBlocked fill:#f8d7da,stroke:#721c24,stroke-width:2px
```

### Implementation Hints

- Initialize runner isolation before runtime turn execution and fail loudly if the configured backend cannot satisfy required policy guarantees.
- Validate global + per-user configs before spawn; create `<workspace_root>/<user_id>/{shared,tmp,vault}` on first-run initialization.
- Pass sidecar connection data only through the length-prefixed JSON bootstrap envelope; at runtime startup, disable shell/browser tools immediately if sidecar socket validation fails.
- In `--insecure` mode, emit a high-visibility warning, report `SandboxTier::Process`, and apply Landlock/Seatbelt as best-effort harm reduction.
- Add approval gates for privileged and dangerous tools even when sandboxing is active; policy and isolation should be layered.

### Finalized Design Decisions

- `oxydra-runner` is the host entry point and only orchestrates per-user `oxydra-vm` + `shell-vm` lifecycle; it never executes user code or joins turns.
- Runtime-side shell/browser availability is determined at startup by validated sidecar socket bootstrap, with explicit disablement on channel failure.
- Isolation posture is explicit via `SandboxTier`; `--insecure` maps to `Process` tier with mandatory warning and best-effort Landlock/Seatbelt.
- `wasmtime` tool isolation is capability-scoped per tool class, with host-function mediated web egress and two-step vault copy semantics.
- Security posture combines runner isolation, policy gates, credential scrubbing, and auditable traces.

### Considerations

- Decide environment-specific fallback behavior (hard fail vs explicitly-degraded mode) and per-user sidecar pooling limits by deployment tier.

---

## Chapter 8: Multi-Agent Orchestration, Routing, and The Gateway Pattern

Once the foundational agent runtime is secured, stateful, and capable of executing tools predictably, the architecture must scale to accommodate multiple concurrent agents, diverse communication channels, and complex workflows. This requires shifting from a single-process execution script to a daemonized control plane utilizing the Gateway Pattern.[1, 30]

### Multiplexing Channels via the Central Control Plane

The Gateway acts as a long-running, highly concurrent background daemon responsible for multiplexing all ingress and egress traffic.[30] It utilizes a multi-threaded asynchronous reactor to manage connections across various interfaces simultaneously, including local `tui` channel connections, dedicated WebSockets, or external third-party messaging APIs such as Telegram, Slack, WhatsApp, and Discord.[1, 5, 30]

When a payload arrives at the Gateway, it is translated from the platform-specific format into a unified internal `Event` struct. The Gateway then consults its dynamic routing configuration to determine the appropriate processing destination.[30, 31] To prevent race conditions and ensure data integrity, messages destined for the same user or specific agent session are queued sequentially in dedicated processing lanes.[1] Conversely, messages intended for distinct users run entirely in parallel.[1] This lane-based queue architecture guarantees that automated background tasks, heavy cron jobs, or proactive heartbeat check-ins do not block or delay real-time conversational threads.[1, 30]

### State Graphs, Handoffs, and Subagent Delegation

The true transformative power of an enterprise-grade agent orchestrator lies in its ability to synthesize networks of specialized subagents.[32] Instead of relying on a single, massive context window to handle coding, research, and client communication simultaneously, the Gateway can route tasks to highly specialized, isolated model instances.[30]

A Master Router agent receives the initial user query, analyzes the required intent, and determines the optimal workflow. It can dynamically spawn subagents, allocating distinct system prompts, specific tool registries, and highly restricted security capability sets to each.[33] For example, a complex research query triggers the creation of a researcher subagent possessing network egress access but zero file-write permissions.[28, 33]

The state management of complex multi-agent handoffs is best modeled using explicit, deterministic state graphs inspired by LangGraph-style patterns while implemented natively in Rust.[33] The orchestrator tracks workflow through nested node states and only transitions when guard conditions are satisfied.[13] Subagent execution must be lifecycle-managed with per-turn timeout, max-turn, cancellation, and token-cost budgets. A structured `SubagentBrief` should carry goal, key facts, tool scope, workspace path, output contract, and budget limits between agents. Throughout this lifecycle, the Gateway keeps the user connection active and streams progress with trace-linked status updates for full observability.[2, 8]

| Orchestration Capability | Mechanism | Architectural Benefit |
|---|---|---|
| Channel Multiplexing | Asynchronous Gateway Daemon | Unifies Slack, Discord, and TUI traffic into a single control plane. |
| Lane-Based Queueing | Per-user Sequential Processing | Prevents background cron tasks from stalling real-time user chat. |
| Subagent Delegation | Master Router Allocation | Optimizes inference costs by using smaller models for specialized tasks. |
| Workflow State Graphs | Graph-based Transition Logic | Ensures deterministic progression of tasks across multiple independent agents. |

### Identity and Portability Standards

As agents become persistent entities across channels, managing persona, memory scope, and behavioral policy becomes a first-class concern. Use a simple native TOML/YAML persona schema as the canonical format for progressive implementation, then add optional compatibility adapters for external identity specs (including AIEOS) where needed.[6] Loading persona definitions at session start ensures consistent tone, capability boundaries, and tool priorities across TUI and external channel interactions.[1, 6]

### User and Channel Onboarding

User onboarding is runner-first and configuration-driven. To register a new user, operators add a `user_id` entry in global runner config with the path to that user's per-user config file, then create the per-user config carrying mount/resource/credential/behavior settings. Runner allocates `<workspace_root>/<user_id>/` on first spawn and initializes `shared`, `tmp`, and `vault` before guest start.

If a configured user has never been started, `oxydra-runner` performs first-run initialization when that user is launched: validate configs, materialize workspace layout, then spawn that user's `oxydra-vm` + `shell-vm` pair (or Process tier when explicitly requested via `--insecure`). The first successful spawn establishes the durable runtime identity for that user; later starts reuse the same workspace root and policy envelope.

Channel onboarding is additive: implement the `Channel` trait, register the adapter in gateway channel registry/config, and activate via explicit gateway reload/restart (no implicit hot binary registration path by default). The `tui` crate follows this same adapter contract and is launched via `oxydra-runner --tui`, which connects to an already-running `oxydra-vm` for the target user; if no running guest exists, runner exits with a clear error.

Per-user channel config must include channel-specific allowed sender identifiers (for example `telegram.allowed_sender_ids`, `slack.allowed_sender_ids`, `whatsapp.allowed_sender_ids`). Gateway ingress is default-deny: if an incoming event's platform sender identifier is missing from that user's allowlist for the channel, the event is rejected and audited, and no agent turn is started.

When an existing user connects through a channel they have not used before, gateway creates a new channel-session binding (after sender allowlist validation) but reuses that user's existing workspace and memory namespace keyed by `user_id`; only transient channel metadata is isolated per connection/session.

### Entities to Implement

- **Gateway Daemon:** Long-running tokio server utilizing WebSocket/HTTP.
- **`Router` Struct:** Evaluates incoming events and assigns them to the correct agent workspace.
- **`ChannelSenderAuthPolicy` Struct:** Per-user per-channel sender allowlists with default-deny evaluation and auditable rejection reasons.
- **`SubagentBrief` Struct:** Contract for bounded cross-agent handoffs (`goal`, `key_facts`, `available_tools`, `workspace_path`, `expected_output_format`, `max_turns`, `token_budget`).
- **Persona Loader:** Native persona schema loader with optional AIEOS compatibility adapter.[6]

### Final Architecture Diagram

```mermaid
graph TD
subgraph Ingress Channels
TUI
Slack
Discord
end
subgraph Gateway Control Plane
Router
Router --> |Spawns| CodeAgent
Router --> |Spawns| WebAgent
end
TUI --> Router
Slack --> Router
Discord --> Router
CodeAgent -.-> |Sandboxed Exec| Tools
WebAgent -.-> |Restricted Sandbox| Network
style Gateway Control Plane fill:#cce5ff,stroke:#004085,stroke-width:2px
style Router fill:#d4edda,stroke:#28a745,stroke-width:2px
```

### Implementation Hints

- Implement lane-based queueing based on a hash of `user_id` (or tenant+user) to ensure deterministic multi-user isolation.
- Enforce lifecycle guards per subagent via `tokio::select!` over completion, timeout, cancellation, and budget exhaustion.
- Pass structured `SubagentBrief` payloads instead of raw transcript copies to reduce token waste and improve delegation clarity.[33]
- Propagate OpenTelemetry trace context through handoffs so one request can be traced across parent and child agents.

### Finalized Design Decisions

- Multi-agent orchestration is implemented with deterministic state graphs and bounded subagent lifecycle controls.
- Persona management is native-schema first, with optional external schema compatibility adapters.
- OpenTelemetry trace continuity across agent handoffs is treated as mandatory for operability.
- MCP remains a roadmap capability, but trait surfaces are intentionally designed for adapter-based integration.

### Considerations

- Enable MCP by default only after operational confidence in local/native tool governance and approval workflows.

---

## Chapter 9: Productization Backlog Integration After Core Runtime

Once the core runtime, isolation model, and channel ingress model are in place, the remaining product backlog should be treated as first-class architecture work rather than ad-hoc TODO notes. These surfaces—model-catalog governance, explicit session lifecycle controls, scheduling, skill loading, and persona file governance—touch correctness, operator trust, and long-term maintainability, so they require explicit phase gates and typed contracts.

### Model Catalog Governance and Snapshot Generation

Model metadata should evolve from a lightweight pinned list into a fully typed schema synchronized from the upstream catalog source (for example models.dev snapshots). Introduce a deterministic snapshot generation command (invokable by operator through `tui`/runner control paths) that produces reviewed, versioned catalog artifacts committed into the repository. This flow must remain operator-driven; the runtime/LLM process should not self-mutate model policy files in-place.

### Explicit Session Lifecycle Controls

Session boundaries must be intentional. Add a first-class "new session" control so users can request a clean conversational slate without changing identity, workspace, or channel bindings. This should generate a new canonical `session_id` while preserving prior sessions for recall/audit, and should avoid implicit rollover rules that create ambiguous memory behavior.

### Scheduler as a Governed Execution Surface

Scheduled execution should be implemented as a bounded control-plane capability, not an unconstrained background thread. Support one-off and periodic entries with explicit ownership by `user_id`, and execute each due run through the same runtime policy envelope (timeouts, budgets, tool policy, audit trails) used for interactive turns. Scheduler management should be exposed through typed operations (`list`, `create`, `delete`) and validated at creation time. A key item to note is that scheduler should be able to support silent execution. E.g. An issue faced often is that any interaction done by scheduled task with an LLM also sends LLM output to the user always. We should be able to support conditional and unconditional silent tasks where user may not get notified. (E.g. "check weather every day at 9 am and notify user only if it is going to rain" should send an output to user only if rain was forecasted)

### Skill Loading from Managed Workspace Folder

Skills should load from a deterministic per-user workspace location (for example `<workspace_root>/<user_id>/skills/`) using markdown manifests. The folder is managed as a governed capability boundary: users may add/remove skill files out-of-band, while runtime exposure to the LLM is mediated by a dedicated skill API, not raw unconstrained filesystem traversal. In this roadmap variant, the LLM may create/load/use skills but cannot delete them.

### Persona Governance via `SOUL.md` and `SYSTEM.md`

Persona and policy framing should be externalized into two operator-visible files: `SOUL.md` (identity/persona envelope) and `SYSTEM.md` (system-behavior directives). These files are loaded at session start and treated as non-editable by the LLM. Any modifications happen through human/operator workflows, and policy loaders must surface provenance and effective precedence in diagnostics.

### Entities to Implement

- **`ModelCatalogSnapshotGenerator`:** Typed schema validator + snapshot writer with reproducible output ordering.
- **`SessionLifecycleManager`:** Canonical session creation/rotation APIs and explicit "new session" command handling.
- **`SchedulerStore` + `SchedulerExecutor`:** Durable schedule definitions with bounded turn execution and policy integration.
- **`SkillRegistry` + `SkillLoader`:** Deterministic skill discovery/loading from managed per-user folder.
- **`PersonaFilesLoader`:** `SOUL.md`/`SYSTEM.md` parsing, validation, and immutable runtime projection.

### Implementation Hints

- Keep all five surfaces typed and auditable from day one; avoid stringly-typed config blobs for lifecycle-critical controls.
- Reuse existing `user_id` + canonical `session_id` mapping in channel flow to avoid split-brain memory behavior.
- Execute scheduled runs as normal runtime turns so cancellation, budgeting, and tracing remain uniform.
- Enforce non-editable persona files with policy checks in both tool execution and any direct file-manipulation surfaces.

### Finalized Design Decisions

- Backlog capabilities are promoted into explicit post-core phases, not left as unscheduled TODO notes.
- Session lifecycle becomes an explicit user-facing control surface with deterministic identity semantics.
- Skills and persona governance are capability-managed assets, not free-form writable runtime internals.
- Model-catalog evolution is operator-driven and snapshot-based to preserve auditability.

### Considerations

- Choose the exact scheduler cadence grammar (`cron`, interval DSL, or both) before Phase 19 implementation to avoid parser rewrites.
- Decide whether skill validation supports a strict schema mode only, or schema + permissive compatibility mode.

---

## Conclusion

Architecting a comprehensive, production-grade AI agent orchestrator requires significantly more than wrapping external API calls in high-level scripting languages. It demands a rigorous, progressive systems engineering approach rooted in efficiency and security. By adopting the "small core" philosophy, developers deliberately restrict the fundamental built-in capabilities of the agent runtime, forcing the underlying language model to dynamically generate, execute, and persist its own extensions rather than relying on bloated, hardcoded frameworks.

Progressively building this architecture in Rust ensures that every layer—from unified multi-provider abstractions to asynchronous streaming and memory persistence—benefits from strict safety and predictable performance. Integrating embedded libSQL hybrid retrieval keeps long-horizon context durable without heavyweight infrastructure while staying compatible with optional Turso-hosted deployments. Layered security (runner-isolated execution, policy gates, credential scrubbing, and auditability) ensures autonomous capability can be deployed with controlled operational risk across Linux, macOS, and fallback platforms.

### Final Readiness Summary

This document now incorporates the factual corrections and architecture hardening decisions directly into the proposal. The design is implementation-ready, with explicit guidance for configuration governance, provider reliability, memory lifecycle management, security controls, observability, and phased execution. Remaining open choices are captured as chapter-level considerations so executive review can focus on policy decisions rather than unresolved technical ambiguities.

---

## Appendix: Recommended Crate Ecosystem and Progressive Build Plan

> **Note:** Fact-check corrections and architecture decisions are integrated directly into each chapter's main guidance. This appendix provides the consolidated crate strategy, configuration approach, and definitive build plan.

---

### Architectural Decision: Channels as a Separate Crate

Channel adapters (Telegram, Discord, Slack, WhatsApp, etc.) should live in their own `channels/` crate, **not** inside `gateway/`. The reasoning:

1. **Dependency isolation:** Each channel adapter pulls in a different SDK/HTTP client (e.g., `teloxide` for Telegram, `serenity` for Discord). Bundling these into the gateway crate forces every build to compile all of them, even during development when you only need TUI. A separate `channels/` crate with per-channel **feature flags** (`features = ["telegram", "discord"]`) keeps compile times lean.
2. **The `Channel` trait belongs in `types/`**, alongside `Provider`, `Tool`, and `Memory` — it's a core abstraction. The concrete adapters (TelegramChannel, DiscordChannel) live in `channels/`.
3. **Gateway depends on channels, not the reverse.** The gateway discovers and manages channel instances, routes messages to/from them, and handles lifecycle. Channels don't know about the gateway.
4. **Testing in isolation:** You can test channel adapters against mock APIs without starting a full gateway.

This matches ZeroClaw's architecture (separate `src/channels/` with per-platform files) and OpenClaw's `extensions/` pattern.

`tui` is an intentional exception: it remains a separate crate but still implements the same `Channel` contract so terminal ingress follows the same routing semantics as external channels. Because this roadmap is still pre-deployment, `cli` -> `tui` is treated as a hard cutover (no compatibility alias crate).

Updated workspace layout:
```
crates/
  types/          # Zero internal deps. Message, ToolCall, Context, Error, trait definitions
  provider/       # Depends on types. Provider implementations + ReliableProvider wrapper
  tools/          # Depends on types. Tool trait + core tools (read, write, edit, bash)
  tools-macros/   # Proc-macro crate for #[tool] attribute (no internal deps except syn/quote)
  runtime/        # Depends on types, provider, tools. Agent loop + state machine
  memory/         # Depends on types. Memory trait + libSQL implementation (embedded + optional Turso remote)
  sandbox/        # Depends on types. Sandbox trait + Wasmtime mount policy + optional in-guest hardening
  runner/         # Depends on runtime, sandbox, types. Host orchestrator for runtime/sidecar sandbox guests
  shell-daemon/   # Depends on sandbox, types. Guest daemon for shell/browser session RPC
  channels/       # Depends on types. Channel trait impls (Telegram, Discord, etc.) behind feature flags
  gateway/        # Depends on runtime, channels. Multi-agent routing, WebSocket server
  tui/            # Depends on types, channels. Local terminal Channel adapter (ratatui)
```

---

### Configuration Management Blueprint

To keep configuration understandable in production, standardize these rules across all crates:

1. **Single source model:** Typed config structs live in `types::config`; all loaders deserialize into the same structs.
2. **Deterministic precedence:** Defaults < system file < user file < workspace file < environment < CLI flags.
3. **Versioned schema:** Include `config_version` and migrate forward explicitly; reject unknown major versions.
4. **Secrets policy:** Secrets are referenced, not duplicated. Provider keys resolve through the 3-tier chain and are always redacted in logs/traces.
5. **Reload policy:** Gateway supports explicit reload for non-secret runtime knobs; provider credentials require explicit operator rotation flow.
6. **Validation gate:** CI includes config fixture tests (valid/invalid) so precedence and validation behavior cannot regress.
7. **Typed model selection:** Configuration accepts typed model references validated against `ModelCatalog`; unknown model strings are rejected at startup.
8. **Runner global config is operator-only:** Keep only workspace root, known-user map (`user_id` + per-user config path), default `SandboxTier`, and guest image refs for `oxydra-vm`/`shell-vm`. *(These are infrastructure-uniform controls and cannot safely vary per user.)*
9. **Runner per-user config is user-scoped:** Per-user files hold mount paths, resource limits, credential references, and user-specific overrides used to launch that user's VM pair.
10. **Bootstrap ownership cutover:** Config loading, provider construction, memory backend wiring, and runtime-limit derivation must move out of legacy standalone `cli` startup code into runner-era `oxydra-vm` bootstrap surfaces before Phase 10 isolation rollout.
11. **Pre-production compatibility stance:** Backward compatibility is intentionally out of scope until first deployment; replace superseded crate/tool surfaces directly rather than carrying transition shims.
12. **Channel sender auth is per-user and default-deny:** Per-user channel config must declare allowed sender IDs per channel; events from unknown sender IDs are rejected before routing and recorded in audit logs.

### External AI Crate Strategy (`rig-core`, `genai`, and direct clients)

Use external crates as accelerators, not as architecture-defining dependencies:

| Option | Recommendation | Where It Fits | Risk Profile |
|---|---|---|---|
| Direct `reqwest` + hand-rolled adapters | **Default for core path** | Provider crate, foundational phases | Highest control, lowest lock-in, more implementation effort |
| `genai` (multi-provider helper) | **Optional adapter** | Provider crate only | Faster provider bring-up, but must stay behind internal `Provider` trait |
| `rig-core` (+ ecosystem crates) | **Selective evaluation only** | Experimental provider/runtime adapters | Rich abstractions and ecosystem integrations, but overlaps with custom runtime and can increase lock-in if adopted as core |

Decision policy: keep the internal `Provider`, `Tool`, `Memory`, and `Runtime` traits as the stable contract. Any third-party crate integration must compile behind feature flags and expose only internal types to upper layers.

### Recommended Rust Crate Ecosystem

For the reader building this system, here is a verified set of crates mapped to the build phases and workspace layers where they are introduced.

| Concern | Recommended Crate(s) | Workspace Layer | Introduced In | Notes |
|---|---|---|---|---|
| Async runtime | `tokio` | all | Phase 1 | Industry standard; use `features = ["full"]` |
| Async trait dispatch | `async-trait` | types | Phase 1 | Required for `dyn` dispatch — native AFIT is not dyn-compatible on stable Rust |
| Error types | `thiserror` | types | Phase 1 | Per-crate error enums with `#[from]` for composition |
| Model catalog ingestion | `serde` + `reqwest` (+ optional codegen) | types / provider | Phase 1-2 | Build typed model registry from pinned models.dev snapshot or equivalent source |
| HTTP client | `reqwest` | provider | Phase 2 | Async, supports streaming responses |
| Multi-provider helper (optional) | `genai` | provider | Phase 2 (optional) | Use only behind internal provider adapters; avoid leaking external types |
| Agent framework interop (optional) | `rig-core` (+ ecosystem crates) | provider / runtime | Phase 3+ (evaluation) | Useful for experiments; keep custom runtime as system of record |
| JSON serialization | `serde` + `serde_json` | types | Phase 1 | Universal; all types derive `Serialize`/`Deserialize` |
| SSE parsing | `eventsource-stream` or manual | provider | Phase 3 | For provider streaming; consider `reqwest-eventsource` |
| JSON Schema generation | `schemars` or `tools-rs` | tools | Phase 4 | `schemars`: `#[derive(JsonSchema)]`; `tools-rs`: `#[tool]` macro |
| JSON Schema validation | `jsonschema` | tools | Phase 6 | In-process validation for self-correction loop |
| Cancellation | `tokio-util` (`CancellationToken`) | runtime | Phase 5 | For abort handling, timeouts, budget enforcement |
| Configuration | `config` or `figment` | types / runner / tui | Phase 7 | TOML-based with env var overrides |
| Embedded SQL engine | `libsql` | memory | Phase 8 | Async API; local-file mode by default with optional Turso remote |
| SQL migration | `refinery` (or equivalent SQL migration runner) | memory | Phase 8 | Versioned schema evolution without data loss |
| Vector search | libSQL/Turso native vector support | memory | Phase 9 | Vector-ready path without extension loading |
| Full-text search | libSQL/SQLite FTS5 | memory | Phase 9 | Built into engine |
| Token counting | `tiktoken-rs` | runtime / memory | Phase 9 | OpenAI-compatible tokenizer for context budgeting |
| Local embeddings | `fastembed-rs` | memory | Phase 9 | ONNX-based, offline-first, no API dependency |
| Runner container orchestration | `bollard` (Docker API) or equivalent OCI wrapper | runner | Phase 10 | Launch per-user `oxydra-vm` + `shell-vm` guests; rootless profiles by default |
| Runner transport | `tokio::net::UnixStream` + `tokio-vsock` (where available) | runner / runtime | Phase 10 | Bootstrap/control channels; length-prefixed JSON envelope for sidecar socket handoff |
| Shell daemon session protocol | `tokio` + `serde_json` framing | shell-daemon / sandbox | Phase 10 | `SpawnSession`, `ExecCommand`, `StreamOutput`, `KillSession` operations |
| Process-tier hardening (Linux) | `landlock` | runner / sandbox | Phase 10 | Best-effort filesystem restriction only when `SandboxTier::Process` (`--insecure`) is active |
| Process-tier hardening (macOS) | Seatbelt profile integration (native) | runner / sandbox | Phase 10 | Best-effort filesystem restriction only when `SandboxTier::Process` (`--insecure`) is active |
| Sandboxing (WASM) | `wasmtime` | sandbox | Phase 11 | Cross-platform capability isolation for file/web/vault tools |
| TUI channel adapter | `ratatui` + `crossterm` | tui | Phase 12 | Local terminal Channel client that connects to a running `oxydra-vm` |
| Telemetry | `tracing` + `tracing-subscriber` | all (from Phase 1) | Phase 1 (basic), Phase 16 (OTel) | Start with `tracing` spans from day one |
| OpenTelemetry | `opentelemetry` + `tracing-opentelemetry` | runtime / gateway | Phase 16 | Distributed traces, metrics, cost attribution |
| Testing | `tokio::test` + `mockall` | all | Phase 5+ | `mockall` for trait mocking (`MockProvider`, `MockTool`) |
| Snapshot testing | `insta` | provider | Phase 5+ | Verify provider serialization doesn't drift |
| WebSocket server | `axum` + `tokio-tungstenite` | gateway | Phase 12 | For baseline gateway daemon + TUI end-to-end path (and later channel WebSocket bridges) |
| Proc macros | `syn` + `quote` + `proc-macro2` | tools-macros | Phase 4 | For `#[tool]` attribute macro |
| Credential scrubbing | `regex` (compiled `RegexSet`) | runtime | Phase 11 | Scrub `api_key`, `token`, `password`, `bearer` from tool output |

---

### Progressive Build Plan

This plan is designed so **no phase requires rewriting work from a previous phase**. Each phase extends (not replaces) the prior phase's output. The "Builds On" column shows dependencies to make this explicit. Every phase is complete only when its verification gate is enforced by automated tests in CI.

| Phase | Focus | Crate(s) Touched | Builds On | Verification Gate | Key Design Constraint |
|---|---|---|---|---|---|
| **Phase 1** | Types + error hierarchy + `tracing` setup | `types` | — | All types serialize/deserialize via serde; `thiserror` errors compile; basic `tracing` subscriber logs to stderr | Define `Message`, `ToolCall`, `Context`, `Response`; `ProviderError`, `ToolError`, `RuntimeError`; `ModelId`/`ProviderId` newtypes. Add `tracing` spans from day one — retrofitting is painful. |
| **Phase 2** | Provider trait + OpenAI chat completions (non-streaming) | `types`, `provider` | Phase 1 | Send a prompt to OpenAI, receive a `Response` struct back; unknown model IDs fail validation against catalog | Use `#[async_trait]`. Define `ProviderCaps` (capabilities struct) and typed model lookup via `ModelCatalog`. Don't hardcode OpenAI wire format into `Message` — keep it generic. |
| **Phase 3** | Streaming support + SSE parsing | `provider` | Phase 2 | Stream tokens to stdout in real-time from OpenAI | `StreamItem` enum: `Text`, `ToolCallDelta`, `ReasoningDelta`, `UsageUpdate`, `ConnectionLost`, `FinishReason`. Use bounded `mpsc` channels. Handle connection drops and resume where supported. |
| **Phase 4** | Tool trait + `#[tool]` macro + core tools (read, write, edit, bash) | `types`, `tools`, `tools-macros` | Phase 1 | `#[tool] async fn read_file(path: String) -> String` compiles and generates correct `FunctionDecl` JSON | Tool trait: `fn schema()` + `async fn execute()` + `fn timeout()` + `fn safety_tier()`. Core tools: Read, Write, Edit, Bash. |
| **Phase 5** | Agent loop (minimal turn cycle) + cancellation + testing | `runtime` | Phases 2–4 | Agent autonomously reads a file when asked; `Ctrl+C` cancels mid-turn; `MockProvider` tests pass | The loop: send context → stream → detect tool calls → execute → re-send. Add `CancellationToken`, per-turn timeout, max-turn budget, and max-cost budget. Write `MockProvider` + `MockTool` for deterministic testing. |
| **Phase 6** | Self-correction loop + parallel tool execution | `runtime` | Phase 5 | Agent recovers from bad JSON arguments; parallel `read_file` calls execute concurrently | Validation guard catches `serde` errors, formats them as tool results, re-invokes provider. Parallel execution for safe-tier tools via `join_all`. |
| **Phase 7** | Second provider (Anthropic) + config management + provider switching | `provider`, `types`, `tui` | Phases 2, 5 | Swap providers via `agent.toml` without code changes; Anthropic block format serializes correctly; config precedence tests pass | Add `config`/`figment` loader with deterministic precedence and `config_version` validation. Introduce `ReliableProvider` wrapper with retry/backoff. Add snapshot tests (`insta`) for provider serialization and credential resolution fixtures (explicit -> provider env -> generic fallback). Phase 10 performs the hard cutover that removes legacy standalone-CLI bootstrap ownership. |
| **Phase 8** | Memory trait + libSQL conversation persistence | `memory` | Phase 1 | Conversations survive process restart; session list/restore works | `Memory` trait: `store`, `recall`, `forget`. Persist with async `libsql` in embedded mode by default. Use versioned SQL migrations from the start (for example `refinery`). Store conversations as JSON blobs initially — don't over-normalize. |
| **Phase 9** | Context window management + hybrid retrieval (vector + FTS) | `memory`, `runtime` | Phases 8, 5 | Agent operates within token budget; rolling summarization triggers correctly; hybrid search returns relevant results | `tiktoken-rs` for token counting. Context budget: system + memory + history + tools + buffer. Use libSQL/Turso vector search plus FTS5 keywords. `fastembed-rs` for local embeddings. Address summarizer race condition with epoch counter. |
| **Phase 10** | Runner and isolation infrastructure | `types`, `runner`, `sandbox`, `tools`, `runtime`, `shell-daemon` | Phase 5 | Runner spawns a per-user `oxydra-vm` + `shell-vm` pair; `oxydra-vm` startup uses runner-era bootstrap wiring for config/provider/memory/runtime limits; runtime connects to shell daemon and a basic shell command returns stdout. `--insecure` starts without VM/container, shell/browser tools report disabled, and Landlock/Seatbelt restriction attempt is logged. | Implement `SandboxTier` (`MicroVm`, `Container`, `Process`) in `types`; implement global/per-user runner configs; wire `oxydra-vm`/`shell-vm` over vsock/unix sockets; pass sidecar endpoint via length-prefixed JSON bootstrap envelope. Implement shell daemon (`SpawnSession`, `ExecCommand`, `StreamOutput`, `KillSession`), plus `ShellSession`/`BrowserSession` with `VsockShellSession` and `LocalProcessShellSession`. Hard-cut bootstrap ownership from legacy standalone `cli` paths. |
| **Phase 11** | Application-layer security policy + WASM tool isolation | `sandbox`, `tools`, `runtime` | Phase 10 | `web_fetch` rejects `169.254.169.254` and `192.168.x.x`; `vault_copyto` emits two distinct auditable WASM invocations; credential-like tokens are scrubbed from tool output; file tools enforce mount boundaries; legacy `read_file`/`write_file`/`edit_file`/`bash` names are not exposed as compatibility aliases. | Implement wasmtime tool runners and full tool set: read/search/list (`shared`+`tmp`+`vault` RO), write/edit/delete (`shared`+`tmp` RW), web tools (no FS mounts, host-function HTTP proxy only, hostname resolution before block checks), vault copy via two host-orchestrated sequential invocations so no single WASM instance mounts both vault and shared/tmp. Add `SecurityPolicy` command allowlist + path traversal blocking and RegexSet output scrubbing. This phase is a hard tool-surface cutover (no backward-compat layer). |
| **Phase 12** | Channel trait + TUI channel interface + baseline gateway daemon | `types`, `channels`, `gateway`, `tui`, `runner` | Phases 5, 8, 10 | `Channel` trait compiles in `types`, `tui` implements it, and the gateway routes TUI traffic end-to-end to a running `oxydra-vm`; output streams correctly, and Ctrl+C cancels only the active turn without killing the guest; if no running guest exists, runner exits with a clear error. | Introduce `Channel` trait here (`send`, `listen`, `health_check`) so TUI is a first-class adapter rather than a standalone runtime. Land a minimal gateway daemon in this phase so TUI uses the same routing path as channel traffic from day one. In `--tui` mode runner performs connect-only behavior instead of spawning a new VM pair. Rename `cli` -> `tui` as a hard cutover with no compatibility alias. |
| **Phase 13** | Full model-catalog schema + snapshot generation workflow | `types`, `provider`, `runner`, `tui` | Phases 1, 2, 12 | Catalog snapshot command produces deterministic typed artifact; config validation rejects unknown/invalid model metadata; snapshot regeneration is reproducible in CI. | Promote pinned model metadata to a complete typed schema and operator-driven snapshot generation command. Runtime/LLM does not self-mutate catalog files. |
| **Phase 14** | First external channel + user/channel session identity mapping | `types`, `channels`, `gateway`, `runtime`, `memory` | Phases 8, 12 | Messages received from the first external channel through gateway routing trigger agent responses; canonical session identity derived from (`user_id`, `channel_id`, `channel_session_id`) maps deterministically to one runtime `session_id`; reconnects reuse the same workspace and memory namespace; sender IDs not in the user's channel allowlist are rejected and audited. | Implement feature-flagged external channel adapters, per-user per-channel sender-ID allowlists, and explicit identity mapping from user/channel sessions to stable memory `session_id` values. Apply default-deny ingress checks before routing. No backward-compat mapping for pre-cutover ad hoc session identifiers is required. |
| **Phase 15** | Multi-agent: subagent spawning + delegation + advanced gateway routing | `runtime`, `gateway` | Phases 5, 12, 14 | One agent delegates a task to a subagent; lane-based per-user queueing works under concurrent multi-channel load | `SubagentBrief` struct for context handoffs. Per-subagent `CancellationToken` + timeout + cost budget. Gateway hardens multiplexing/queue policy across TUI + external channels on top of the Phase 12 daemon baseline. |
| **Phase 16** | Observability: OpenTelemetry traces + metrics + cost reporting | `runtime`, `gateway` | Phases 5, 15 | Full trace of agent turns visible in Jaeger; per-session cost report; tool success/failure rates | Upgrade from `tracing` subscriber to `tracing-opentelemetry` exporter. Add token cost metrics. Trace spans: turn → provider_call → tool_execution. Conversation replay from stored traces. |
| **Phase 17** | MCP (Model Context Protocol) support | `tools`, `runtime` | Phases 4, 5 | External MCP tool servers discoverable and callable alongside native tools | `McpToolAdapter` implements `Tool` trait, bridging MCP protocol (stdio or HTTP+SSE transport). MCP tools registered in the same tool registry as native tools. Discovery via config or runtime negotiation. |
| **Phase 18** | Explicit session lifecycle controls (`new session`) | `runtime`, `memory`, `types`, `tui`, `channels` | Phases 8, 13, 14 | User can start a clean new session explicitly; previous sessions remain recallable; `session_id` rollover is deterministic and auditable. | No implicit session rollover heuristics; new-session control is explicit and preserves user/workspace continuity with fresh conversation state. |
| **Phase 19** | Scheduler system + schedule management tooling | `runtime`, `memory`, `types`, `tools`, `gateway` | Phases 10, 15, 18 | One-off and periodic schedule entries can be listed/created/deleted; due jobs execute bounded turns with normal policy/audit controls. | Scheduler runs must execute through the same policy envelope as interactive turns (timeouts, budgets, tool gates, tracing). |
| **Phase 20** | Skill system from managed workspace folder | `runtime`, `tools`, `types`, `memory` | Phases 11, 18, 19 | Skills are discovered/loaded from managed per-user folder; LLM can create/load/use skills; skill deletion by LLM is denied by policy. | Skill access is API-mediated and policy-scoped; avoid exposing raw unmanaged skill-folder traversal as a generic file operation. |
| **Phase 21** | Persona governance via `SOUL.md` + `SYSTEM.md` | `runtime`, `types`, `runner`, `gateway` | Phases 12, 18, 20 | Persona files load at session start with visible effective precedence; LLM write/delete attempts on these files are rejected; operator edits apply on next session start. | `SOUL.md` and `SYSTEM.md` are non-editable runtime policy artifacts and must remain outside direct LLM mutation paths. |

#### TODOs not covered in above phases yet
- No unscheduled backlog items remain from the previous list; these concerns are now explicitly planned in Phases 13-21.

### Test Strategy Across All Phases

- Every phase verification gate must be represented by automated tests before phase closure.
- Early phases (1-4) prioritize type/serialization tests, schema tests, and provider contract tests.
- Middle phases (5-11) add runtime integration tests for cancellation, retries, tool safety tiers, memory migration, runner isolation, and WASM policy behavior.
- Later phases (12-21) add end-to-end and distributed tests for TUI/channel adapters, gateway routing, observability, MCP adapters, model-catalog governance, session controls, scheduler flows, skills, and persona policy enforcement.
- Regression policy is cumulative: later phases add tests but do not remove earlier phase checks.

#### Why This Order Avoids Rewrites

- **Phase 1 defines the canonical types** that every subsequent phase uses. `Message`, `ToolCall`, `Context` are stable from day one.
- **Phase 2 introduces the `Provider` trait** with `#[async_trait]` and `ProviderCaps` — Phase 7 adds a second provider without changing the trait.
- **Phase 4 defines the `Tool` trait** with `safety_tier()` and `timeout()` — Phase 6 uses these for parallel execution decisions, Phase 11 uses `safety_tier()` for `wasmtime` mount/egress policy, and Phase 17 implements it for MCP.
- **Phase 5 builds the agent loop with `CancellationToken`** — Phase 15 reuses the same token for subagent lifecycle management.
- **`tracing` is introduced in Phase 1** as simple stderr logging — Phase 16 upgrades the subscriber to OpenTelemetry without changing any instrumentation code.
- **Phase 8 uses versioned SQL migrations from day one** — Phase 9 adds vector/FTS tables via migration, not schema wipe.
- **Phase 10 performs the bootstrap cutover** from legacy standalone `cli` startup ownership to runner-era `oxydra-vm` startup wiring before isolation controls are enforced.
- **Phase 12 defines the `Channel` trait** and lands a gateway-backed TUI end-to-end path before external adapters land — Phase 14 extends adapters without changing the trait.
- **Phase 14 introduces deterministic user/channel -> session identity mapping and sender-ID allowlists** before Phase 15 concurrency hardening, preventing multi-channel history fragmentation and unauthorized ingress.
- **Phases 13 and 18 formalize catalog/session governance** on top of earlier typed IDs and memory/session primitives, preventing implicit state drift.
- **Phase 19 scheduler execution reuses runtime policy rails** (budget, cancellation, tool gating) instead of inventing a separate unsafe execution path.
- **Phases 20-21 separate skills from persona policy assets,** avoiding policy-file mutation through general skill flows.

### Phase-to-Chapter Alignment Check

- **Chapters 1-2** map to **Phases 1-4 and 7** (types, provider abstraction, tool payload normalization, and configuration/provider switching).
- **Chapters 3-5** map to **Phases 5-7** (runtime loop, cancellation, streaming robustness, tools, and schema self-correction).
- **Chapter 6** maps to **Phases 8-9** (durable memory, retrieval, token budgeting, summarization).
- **Chapter 7** maps to **Phases 10-11** (runner isolation infrastructure and application-layer security policy).
- **Chapter 8** maps to **Phases 12 and 14-17** (channel trait, TUI/external channel adapters, gateway orchestration, observability, MCP compatibility).
- **Chapter 9** maps to **Phases 13 and 18-21** (model-catalog governance, explicit session lifecycle, scheduler, skills, and persona file policy).

This crosswalk confirms the progressive plan is aligned with the narrative architecture and covers the required execution surfaces without forcing rework between phases.

---

*End of Appendix*
