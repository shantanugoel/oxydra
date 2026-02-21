# Chapter 4: Tool System

## Overview

The tool system bridges the strongly-typed Rust world with the loosely-structured JSON generation of LLMs. It provides automatic schema generation, validation with self-correction, safety classification, and policy enforcement — ensuring that even malformed or malicious tool calls are handled safely.

## The Tool Trait

Defined in `types/src/tool.rs`:

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn schema(&self) -> FunctionDecl;
    async fn execute(&self, args: &str) -> Result<String, ToolError>;
    fn timeout(&self) -> Duration;
    fn safety_tier(&self) -> SafetyTier;
}
```

- `schema()` — returns the JSON Schema declaration that tells the LLM how to call this tool
- `execute()` — receives raw JSON arguments as a string, validates and executes
- `timeout()` — maximum execution duration (enforced by the registry)
- `safety_tier()` — classification that determines execution policy

## Safety Tiers

```rust
pub enum SafetyTier {
    ReadOnly,       // No side effects (file_read, file_search, file_list)
    SideEffecting,  // Modifies state within sandbox (file_write, file_edit, web_fetch)
    Privileged,     // High-risk operations (shell_exec)
}
```

Safety tiers drive two behaviors:
1. **Parallel execution** — `ReadOnly` tools run concurrently; others run sequentially
2. **Approval gates** — `Privileged` tools can be gated behind user confirmation callbacks

## Schema Types

```rust
pub struct FunctionDecl {
    pub name: String,
    pub description: Option<String>,
    pub parameters: JsonSchema,
}

pub enum JsonSchemaType {
    Object { properties: BTreeMap<String, JsonSchema>, required: Vec<String> },
    String { enum_values: Option<Vec<String>>, description: Option<String> },
    Integer { description: Option<String> },
    Number { description: Option<String> },
    Boolean { description: Option<String> },
    Array { items: Box<JsonSchema>, description: Option<String> },
}
```

## The `#[tool]` Procedural Macro

**Crate:** `tools-macros`

The `#[tool]` attribute macro automates `FunctionDecl` generation from Rust function signatures:

```rust
#[tool]
/// Reads the contents of a file at the given path.
async fn file_read(path: String) -> String {
    // implementation
}
```

This generates a hidden function `__tool_function_decl_file_read()` that builds the `FunctionDecl` with:
- Function name as tool name
- Doc comments as the tool description
- Parameter names and types mapped to JSON Schema types

### Type Mapping

| Rust Type | JSON Schema Type |
|-----------|-----------------|
| `String`, `&str` | `String` |
| `bool` | `Boolean` |
| `i8`..`u128`, `isize`, `usize` | `Integer` |
| `f32`, `f64` | `Number` |

### Constraints

- Functions must be `async`
- Generics are not supported
- Methods with `self` receivers are not supported
- Complex patterns in parameters are not supported
- Compile-time errors are generated for violations (verified via `trybuild` tests)

## Core Tools

| Tool Name | Safety Tier | Description |
|-----------|-------------|-------------|
| `file_read` | `ReadOnly` | Reads UTF-8 text from a file |
| `file_search` | `ReadOnly` | Recursively searches for text patterns in files |
| `file_list` | `ReadOnly` | Lists directory entries |
| `file_write` | `SideEffecting` | Creates or overwrites a file with content |
| `file_edit` | `SideEffecting` | Replaces a specific text snippet in a file |
| `file_delete` | `SideEffecting` | Deletes a file or directory |
| `web_fetch` | `SideEffecting` | Fetches URL content (converts to Markdown/Text) |
| `web_search` | `SideEffecting` | Performs web searches via configured providers |
| `vault_copyto` | `SideEffecting` | Securely copies data from vault to shared/tmp workspace |
| `shell_exec` | `Privileged` | Executes shell commands (via sidecar or host) |

Most tools (except `BashTool`) execute through the `HostWasmToolRunner`, which enforces capability-based mount policies per tool class (see Chapter 7: Security Model).

### BashTool Backends

The `BashTool` operates in three modes determined by the bootstrap environment:

1. **Host** — direct execution via `std::process::Command` (local development)
2. **Session** — forwarded to a `ShellSession` in the shell-daemon sidecar (production)
3. **Disabled** — rejects all execution if no safe execution environment is available

## Tool Registry

**File:** `tools/src/registry.rs`

The `ToolRegistry` manages tool lifecycle and enforces execution policies.

### Registration

Tools are registered at startup via `bootstrap_runtime_tools`, which reads the `RunnerBootstrapEnvelope` to determine which tools are available based on the current sandbox tier and sidecar status.

### Execution Pipeline (`execute_with_policy`)

Every tool call passes through this pipeline:

```
1. Tool Lookup      → Find tool by name in registry
2. Safety Gate      → Check SafetyTier against caller-provided approval policy
3. Security Policy  → WorkspaceSecurityPolicy validates arguments:
                      - Filesystem: path canonicalization + root boundary check
                      - Shell: command allowlist + syntax restriction
4. Timeout Wrap     → tokio::time::timeout(tool.timeout())
5. Execute          → tool.execute(args)
6. Output Truncate  → Cap output at max_output_bytes (default 16KB)
7. Return           → Truncated result or formatted error
```

### Output Truncation

If tool output exceeds `max_output_bytes` (16KB default), it is truncated and a suffix is appended:

```
[output truncated — original size: 142,857 bytes]
```

This prevents context window exhaustion from verbose tool output.

## Validation and Self-Correction

When an LLM generates malformed tool call arguments, the system does not crash. Instead:

1. The runtime's `ToolCallAccumulator` reconstructs the full argument string from streaming deltas
2. `serde_json::from_str` attempts to parse the arguments
3. If parsing fails, `jsonschema` validates against the tool's declared schema
4. The validation error is formatted as a `MessageRole::Tool` result:
   ```
   Tool execution failed: missing required field 'path' in arguments
   ```
5. This error message is appended to the conversation context
6. The provider is re-invoked, giving the LLM a chance to self-correct

This transforms what would be a fatal runtime crash into a recoverable iteration step. The model sees its mistake and typically corrects it on the next turn.
