# Attachment Save Feature Plan

## Goal

Allow agents to persist inbound user attachments (especially Telegram images/files) to workspace paths (`/shared`, `/tmp`) in a safe, explicit, and auditable way.

## Problem Statement

Current flow already ingests attachment bytes from Telegram and sends them to provider context as inline attachments, but tools cannot directly access those inbound bytes. `file_write` is text-oriented (`content: string`) and does not solve the missing attachment-byte access path by itself.

## Success Criteria

1. Agent can reference inbound attachment(s) from the current turn and save them to a user-visible workspace path.
2. Save behavior is explicit and deterministic (no hidden auto-writes).
3. Security boundaries remain intact (path controls, size limits, MIME checks, allowed mount points).
4. End-to-end works for Telegram and remains channel-agnostic for future channels.
5. Tests cover unit + integration + channel path with failure cases.

## Design: New tool `attachment_save`

- Introduce a dedicated side-effecting tool that saves one inbound attachment to a target path.
- Tool arguments: attachment selector (`index`), destination `path`, optional `overwrite` flag.
- Tool reads inbound attachment metadata+bytes from `ToolExecutionContext` and writes via the WASM sandbox runner using the `FileReadWrite` capability profile.

**Why a dedicated tool (vs extending `file_write` or auto-materializing):**

- Clear semantics: saving an inbound attachment is a first-class action.
- No ambiguity in `file_write` contract (keeps text I/O simple).
- Easier to enforce attachment-specific checks and better UX errors.
- Easier auditability and future extension (e.g., dedupe, virus scan hook).

## Tool Contract

**Tool name:** `attachment_save`

**Safety tier:** `SideEffecting`

**Args:**

| Parameter   | Type    | Required | Default | Description |
|-------------|---------|----------|---------|-------------|
| `index`     | integer | yes      | —       | Zero-based index from inbound attachment list for current user turn |
| `path`      | string  | yes      | —       | Destination path in `/shared` or `/tmp` |
| `overwrite` | boolean | no       | `false` | Fail if file exists unless `true` |

**Return shape (JSON string):**

```json
{
  "saved": true,
  "path": "/shared/photo.jpg",
  "mime_type": "image/jpeg",
  "bytes_written": 142857,
  "source_index": 0
}
```

**Errors:**

- Index out of bounds
- No inbound attachments for current turn
- Destination outside allowed mounts (`/shared`, `/tmp`)
- Destination exists and `overwrite=false`
- Size exceeds configured max
- Write failure

## Data Plumbing Changes

### Extending `ToolExecutionContext`

Add an inbound attachment field to `ToolExecutionContext` (`types/src/tool.rs`):

```rust
/// Inbound attachments from the current user turn, available for tools
/// like `attachment_save`. Ref-counted to avoid copying large payloads
/// when the context is cloned.
pub inbound_attachments: Option<Arc<Vec<InlineMedia>>>,
```

**Key detail:** Use `Arc<Vec<InlineMedia>>` (not `Vec<InlineMedia>`) because `ToolExecutionContext` is `Clone` and is cloned in `runtime/src/lib.rs` when augmenting with `event_sender`/provider. With up to 40MB of attachment payload per turn, each clone would otherwise duplicate the full byte buffers. Cloning an `Arc` is O(1).

### Populating attachments in the gateway turn runner

In `gateway/src/turn_runner.rs`, the `UserTurnInput.attachments` are already available when constructing `ToolExecutionContext`. Wrap them in `Arc` and assign:

```rust
let tool_context = ToolExecutionContext {
    // ... existing fields ...
    inbound_attachments: if attachments.is_empty() {
        None
    } else {
        Some(Arc::new(attachments.clone()))
    },
};
```

The attachments are also pushed into `Message.attachments` for provider multi-modal input (existing behavior, unchanged).

### Turn scoping

- `ToolExecutionContext` is constructed fresh each turn — attachments are naturally scoped to the current turn.
- The existing logic in `turn_runner.rs` (lines 133–139) that clears previous-turn `Message.attachments` remains unchanged.
- No attachment bytes leak into memory/history persistence.

### LLM attachment awareness (prompt augmentation)

Tool existence alone is insufficient — the LLM needs to know saveable attachments exist for the current turn. Inject structured metadata into the turn context (in the gateway turn runner, alongside the user message):

```
User sent 2 attachments: [0] image/jpeg (~245KB), [1] application/pdf (~1.2MB).
Use attachment_save(index, path) to persist any of them to /shared or /tmp.
```

This augmentation is added as a system-role or user-role addendum only when the turn has attachments, so it does not pollute turns without attachments.

### Deferred: handle-based indirection

Move from "bytes in context" to "opaque handle store" only if evidence appears of:
- Frequent large attachments near caps causing elevated RSS
- Multiple context clones per tool call visible in traces
- Need for cross-turn attachment references

## WASM Runner and Write Semantics

### Capability profile

Use `WasmCapabilityProfile::FileReadWrite` — matching `file_write`/`file_edit`/`file_delete`. This grants write access to `shared/` and `tmp/` but not `vault/`.

### Binary write operation

The existing WASM guest `file_write` operation is text-oriented. A `file_write_bytes` operation may need to be added to the WASM guest to handle raw binary payloads. If the WASM guest already supports a suitable byte-write operation, use it directly. **This is a scope item to verify during Phase 2.**

### Write atomicity

- For `overwrite=false`: use `O_CREAT | O_EXCL` semantics (`OpenOptions::new().create_new(true)`) to avoid TOCTOU races between existence check and write.
- For `overwrite=true`: use atomic write (temp file + rename in same directory) to prevent half-written files on crash/cancellation.
- Auto-create parent directories (mirroring `file_write` behavior for consistency).

## Security & Policy

### Path field registration (`tools/src/path_spec.rs`)

**Critical:** Register `attachment_save` in `tool_path_fields()`:

```rust
"attachment_save" => READ_WRITE_PATH_FIELD,
```

Without this, `WorkspaceSecurityPolicy` will treat the tool as having **no path-bearing args**, silently bypassing path boundary enforcement. The existing `path_spec` unit tests must also be updated to assert this mapping.

### Policy enforcement

1. Write path constraints identical to `file_write`: only `/shared` and `/tmp`.
2. Reuse existing path canonicalization + root boundary checks (`WorkspaceSecurityPolicy`).
3. Enforce `overwrite=false` by default to reduce accidental data loss.
4. Preserve per-turn attachment size/count limits from gateway and channel adapters.
5. Optional future: MIME allowlist per channel/tool and executable-bit restrictions.

## Design Decisions (resolved from open questions)

| Question | Decision | Rationale |
|----------|----------|-----------|
| Index-only vs `attachment_id` in v1 | Index-only | Attachments are scoped to current turn; stable IDs add no value yet |
| Auto-create parent directories | Yes, mirror `file_write` | Consistency with existing write tools |
| Preserve original extension when path has none | No — write exact path given | LLM can construct proper filenames; simpler and more predictable semantics |
| Per-MIME restrictions for save in v1 | No | Gateway already validates MIME format; defer to v2 if needed |

## Testing Plan

### Unit tests

1. `attachment_save` schema correctness.
2. Successful save writes expected bytes.
3. Rejects invalid index.
4. Rejects when no attachments exist in context.
5. Rejects out-of-scope path.
6. Overwrite=false rejects existing file; overwrite=true succeeds.
7. Atomic write: partial write on crash does not leave corrupt file.

### Integration tests

1. Runtime tool execution with context-carried attachment saves file correctly.
2. Gateway-to-runtime plumbing ensures attachment list appears in tool context.
3. Memory persistence path still strips attachment bytes from stored chat history (no regression).
4. Prompt augmentation injects attachment metadata only when attachments are present.

### Channel tests (Telegram)

1. Inbound photo/file arrives as attachment; agent can invoke `attachment_save` and persist.
2. Large attachment rejection remains intact.

### Docs updates

1. Guidebook Chapter 4 (tool catalog and semantics).
2. Guidebook Chapter 9/12 where channel attachment handling is described.

## Rollout Plan (Incremental)

### Phase 1: Context plumbing

1. Add `inbound_attachments: Option<Arc<Vec<InlineMedia>>>` to `ToolExecutionContext`.
2. Update `Debug` impl to show attachment count (not bytes).
3. Populate in gateway turn runner from `UserTurnInput.attachments`.
4. Add prompt augmentation logic for attachment metadata injection.
5. Add tests for context propagation and prompt augmentation.

### Phase 2: Tool implementation

1. Verify/add `file_write_bytes` WASM guest operation if needed.
2. Implement `attachment_save` tool with schema + validation + atomic write semantics.
3. Register tool in `register_runtime_tools()` (`tools/src/registry.rs`).
4. Register path field in `tool_path_fields()` (`tools/src/path_spec.rs`) + update unit tests.
5. Add unit tests for tool behavior, errors, and overwrite semantics.

### Phase 3: End-to-end hardening

1. Add integration tests with runtime (context → tool → file on disk).
2. Verify `parse_policy_args` in `registry.rs` picks up the `path` field correctly.
3. Update guidebook docs and tool guidance prompt text.
4. Verify no regressions in `file_write`, `send_media`, or memory stripping behavior.
5. Verify clippy/tests pass across all touched crates.

## Risk Assessment

**Low-to-medium engineering risk:**

| Risk | Mitigation |
|------|------------|
| Large payload data in tool context (up to 40MB) | `Arc` wrapping avoids copy-on-clone; bounded by existing gateway attachment caps |
| Security bypass via missing path registration | Explicit `path_spec.rs` entry + unit test assertion |
| LLM fails to call tool (doesn't know attachments exist) | Prompt augmentation with structured attachment metadata |
| Half-written files on crash/cancellation | Atomic write semantics (temp + rename / create_new) |
| WASM guest lacks binary write operation | Scope check in Phase 2; add if missing |

## Acceptance Checklist

- [ ] `attachment_save` available in tool registry and callable
- [ ] `ToolExecutionContext` carries `Arc<Vec<InlineMedia>>` for current-turn attachments
- [ ] `tool_path_fields` includes `attachment_save` → `ReadWrite` for `path`
- [ ] Prompt augmentation injects attachment metadata when attachments are present
- [ ] Writes use atomic semantics (create_new for non-overwrite, temp+rename for overwrite)
- [ ] Inbound attachment from Telegram can be saved to `/shared/...`
- [ ] Invalid usage returns clear, actionable errors
- [ ] No regressions in existing `file_write`, `send_media`, or memory stripping behavior
- [ ] All tests and clippy pass for touched crates
