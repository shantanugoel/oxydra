# Implementation Plan: Phase 6 - Self-Correction Loop & Parallel Tool Execution

## Objective
Implement Phase 6 of the architecture to introduce an aggressive in-process validation layer (validation guard) and parallel execution of safe-tier tools. This ensures that the agent recovers gracefully from bad tool arguments (self-correction) and executes read-only operations concurrently to optimize runtime performance.

## Prerequisites & Context
- **Base Architecture**: The `runtime` crate implements the `AgentRuntime` and loop logic (Phase 5).
- **Tool Integration**: Tools already specify their `SafetyTier` (`ReadOnly`, `SideEffecting`, `Privileged`).
- **Error Handling**: `ToolError` is already exposed through `RuntimeError::Tool`, which will be caught rather than crashing the session.

## Detailed Steps

### Step 1: Integrate `jsonschema` for Validation Guard
1. **Dependency Addition**: Ensure `jsonschema` is present in `crates/runtime/Cargo.toml` for `serde` error catching and struct validation. (Already added manually). Ensure `futures` crate is available for `join_all`.
2. **Compile Schemas on Demand**: Inside `AgentRuntime::execute_tool_call` (or a new `execute_tool_call_with_validation` wrapper), resolve the tool from `ToolRegistry` and dynamically generate its schema JSON representation. Compile this to a `jsonschema::JSONSchema`.
3. **Validate Arguments**: Validate the raw `arguments` JSON from `ToolCall` against the compiled schema before executing the tool. 
4. **Format Error Payload**: If `jsonschema::validate` yields errors, capture these into a structured string. Return `RuntimeError::Tool(ToolError::InvalidArguments { ... })` explaining the validation failure exactly as required by the LLM to self-correct.

### Step 2: Implement the Self-Correction Catch
1. **Update `execute_tool_call` Return Handling**: Instead of throwing `RuntimeError::Tool` up to `run_session` and crashing the agent loop, match the execution result.
2. **Inject Error as Context**:
   - If execution succeeds: return `Ok(output_string)`.
   - If execution fails with `RuntimeError::Tool(err)` (which encompasses invalid schemas, un-escaped JSON arguments, or execution failures): format a self-correction message like `"Tool execution failed: {err}"`.
   - If execution fails with `RuntimeError::Cancelled` or `RuntimeError::BudgetExceeded`: bubble this up normally as it indicates a fatal operational boundary.
3. **Turn Loop Reinjection**: Since `execute_tool_call` will now return valid contextual tool outputs (even when containing errors), `context.messages.push` will append the tool error directly into the stream, and the turn loop inherently recurses back to the `Provider`, triggering the self-correction.

### Step 3: Implement Parallel Tool Execution (`join_all`)
1. **Partitioning Strategy**: When `run_session` iterates through the `tool_calls` payload, we must not execute all tools sequentially.
2. **SafetyTier Grouping**: Iterate over `tool_calls` in chunks:
   - Identify contiguous blocks of `SafetyTier::ReadOnly` tools.
   - For `ReadOnly` tools: group them into an execution queue.
   - For `SideEffecting` / `Privileged` tools: they mark a synchronous execution boundary.
3. **Concurrent Execution via `join_all`**:
   - When hitting a non-read-only tool (or at the end of the `tool_calls` list), drain the current queue of `ReadOnly` tools and execute them simultaneously using `futures::future::join_all`.
   - Await the collection, format each into a `MessageRole::Tool` context object, and append to the session history in order.
   - Then execute the side-effecting tool synchronously, wait for it, and append its result to the context.
   - Repeat for remaining tool calls.
4. **Context Integrity**: Ensure the order of messages pushed to `context.messages` matches the original order of `tool_calls` requested by the agent, so semantic causality is preserved.

### Step 4: Verification and Testing
1. **Validation Recovery Test**: Write an automated test inside `crates/runtime/src/lib.rs` (or `tests`) simulating an agent that returns malformed JSON arguments (e.g., missing required fields). Verify the validation guard catches it and that the injected tool error is added to the conversation context without crashing.
2. **Parallel Tool Test**: Ensure that parallel tools are executed properly. This can be tested using the mock `slow_tool` (which sleeps) simulating multiple `read_file` calls. Ensure that total time is approximately $O(1)$ tool execution time instead of $O(N)$ sequential time, while side-effecting tools are $O(N)$.

## Relevant File Changes
- **`crates/runtime/Cargo.toml`**: `jsonschema` and `futures`.
- **`crates/runtime/src/lib.rs`**: Update `run_session`'s loop to use `join_all` over read-only tool groups. Enhance `execute_tool_call` to perform `jsonschema` validation against `tool.schema().parameters`.

## Success Criteria (Verification Gate)
- Agent recovers from bad JSON arguments via the injected `serde` / validation error instead of exiting.
- Parallel `read_file` (or any `SafetyTier::ReadOnly`) calls execute concurrently via `join_all`.