# Chapter 7: Security Model

## Threat Model

In agentic AI, threats frequently originate from within — the user's own prompts or poisoned data retrieved by the agent (Indirect Prompt Injection). An attacker can instruct an unsuspecting agent to exfiltrate SSH keys, credentials, or sensitive files. Oxydra does not rely on the LLM's semantic understanding to refuse dangerous commands. Instead, it enforces deterministic, system-level guardrails at every boundary.

## Defense Layers

```
┌──────────────────────────────────────────────────┐
│ Layer 1: Runner Isolation (VM/Container/Process) │
│  - Per-user guest pairs (oxydra-vm + shell-vm)   │
│  - Workspace namespace isolation                  │
├──────────────────────────────────────────────────┤
│ Layer 2: WASM Capability Isolation               │
│  - Per-tool mount policies                       │
│  - No filesystem for web tools                   │
│  - Two-step vault operations                     │
├──────────────────────────────────────────────────┤
│ Layer 3: Security Policy Enforcement             │
│  - Path canonicalization + boundary checks       │
│  - Shell command allowlist + syntax restriction   │
│  - SSRF protection (IP blocking)                 │
├──────────────────────────────────────────────────┤
│ Layer 4: Output Scrubbing                        │
│  - Keyword-based credential redaction            │
│  - Entropy-based secret detection                │
├──────────────────────────────────────────────────┤
│ Layer 5: Runtime Guards                          │
│  - Turn/cost/timeout budgets                     │
│  - Safety tier approval gates                    │
└──────────────────────────────────────────────────┘
```

## Sandbox Tiers

```rust
pub enum SandboxTier {
    MicroVm,    // Highest isolation — Firecracker or equivalent
    Container,  // Medium isolation — Docker/OCI with namespace separation
    Process,    // Development only — host process with best-effort hardening
}
```

The active tier is surfaced in startup logs and health/status endpoints. In `MicroVm` and `Container` tiers, VM/container boundaries plus WASM capabilities are the primary controls. In `Process` tier (enabled via `--insecure`):
- Runner starts Oxydra as a host process
- Shell/browser tools are auto-disabled
- Best-effort Landlock (Linux) or Seatbelt (macOS) filesystem restrictions are applied
- A prominent warning is emitted that isolation is degraded

**Process tier applies the same WASM mount policies and security policies as Container/MicroVM tiers.** The `--insecure` flag means only that the isolation boundary is software-enforced (policy checks + best-effort OS sandboxing) rather than hardware-enforced (container namespaces, VM boundaries). Tools are still restricted to `shared/`, `tmp/`, and `vault/` subdirectories, and the `.oxydra/` internal directory is denied at the policy level. The workspace subdirectory structure (`shared/`, `tmp/`, `vault/`, `.oxydra/`) is provisioned identically across all tiers.

## WASM Tool Isolation

**File:** `tools/src/sandbox/wasm_runner.rs`

**Current state: simulated isolation.** The `HostWasmToolRunner` enforces capability-scoped mount policies and path boundary checks, but executes operations directly on the host using `std::fs` rather than within a `wasmtime` WASM sandbox. The `wasmtime` crate is not currently a dependency. Policy enforcement (path canonicalization, mount boundary checks, capability profiles) is real and functional — but the isolation boundary is logical, not hardware-enforced.

This means that while the security policy correctly rejects out-of-bounds paths and enforces read-only vs. read-write access, a sufficiently creative exploit (e.g., TOCTOU race between canonicalization and access) could bypass these checks. True WASM sandboxing would provide a hard isolation boundary where the guest code physically cannot access unmounted paths.

### Mount Profiles

The policy profiles are defined and enforced, even though execution is host-based:

| Profile | Tools | Mounts | Access |
|---------|-------|--------|--------|
| `FileReadOnly` | `file_read`, `file_search`, `file_list` | `shared`, `tmp`, `vault` | Read-only |
| `FileReadWrite` | `file_write`, `file_edit`, `file_delete` | `shared`, `tmp` | Read-write (no vault) |
| `Web` | `web_fetch`, `web_search` | None | No filesystem access |
| `VaultReadStep` | `vault_copyto` (step 1) | `vault` | Read-only |
| `VaultWriteStep` | `vault_copyto` (step 2) | `shared`, `tmp` | Read-write |

### Execution Path

When a tool is invoked through `HostWasmToolRunner`:

1. `enforce_capability_profile` selects the mount profile for the tool class
2. `enforce_mounted_path` canonicalizes the target path via `fs::canonicalize` and verifies it falls within an allowed mount root
3. Access mode (read-only vs. read-write) is checked against the profile
4. If all checks pass, the operation executes via `std::fs` (e.g., `fs::read_to_string`, `fs::write`)

### Vault Copy Two-Step Semantics

Moving data from the vault to the workspace requires two distinct, auditable invocations:

1. **Read step** — uses `VaultReadStep` profile (vault read-only), reads the source file, returns content to host
2. **Write step** — uses `VaultWriteStep` profile (shared/tmp read-write, no vault access), writes content to destination

No single invocation has concurrent access to both `vault` and `shared`/`tmp`. Both steps are linked by a unique `operation_id` for audit logging. This two-step semantic is enforced at the policy level today; true WASM isolation would make it physically impossible for a single guest to access both.

### Path to Real WASM Isolation

The planned upgrade:
- The `wasmtime` dependency is already in `crates/tools` behind the `wasm-isolation` feature flag
- Compile tool operations as WASM modules with explicit WASI capability grants
- Mount directories via WASI `preopens` so the guest physically cannot access unmounted paths
- Web tools use host-function imports for HTTP access (already designed as a host-function HTTP proxy pattern)
- The `WasmToolRunner` trait and capability profiles remain unchanged — only the execution backend changes

## Security Policy

**File:** `tools/src/sandbox/policy.rs`

The `WorkspaceSecurityPolicy` trait and implementation enforce boundaries before tool execution:

### Path Traversal Protection

1. **Canonicalization** — `fs::canonicalize` resolves all symlinks, `..` components, and relative paths
2. **Root boundary check** — the canonical path must start with one of the allowed roots (`shared`, `tmp`, `vault`)
3. **Access mode check** — `file_write` cannot target `ReadOnly` roots

If a path fails any check, the tool call is blocked with `SecurityPolicyViolationReason::PathOutsideAllowedRoots`.

### Shell Command Restriction

Shell commands are validated in two stages:

**Syntax scrubbing** — commands containing these operators are rejected:
- `&&`, `||`, `;` (command chaining)
- `|` (pipes)
- `>`, `<` (redirection)
- `$()`, backticks (subshell expansion)
- Newlines (multi-line commands)

**Executable allowlist** — only these binaries are permitted:
```
awk, cat, cargo, cp, curl, diff, du, find, git, grep, head, jq,
ls, make, mkdir, mv, node, npm, pnpm, python, python3, rm, rg,
sed, sort, tail, tar, tee, touch, tree, uniq, wc, wget, xargs, yarn
```

Any command not matching the allowlist or containing blocked syntax is rejected before execution.

## SSRF Protection

**File:** `tools/src/sandbox/wasm_runner.rs` (web tools)

Web tools (`web_fetch`, `web_search`) use a host-function HTTP proxy pattern with strict SSRF protections:

### URL Validation

Only `http` and `https` schemes are accepted.

### IP Resolution Before Request

Hostnames are resolved to all associated IP addresses **before** any HTTP request is made. This prevents DNS rebinding attacks.

### Blocked IP Ranges

| Range | Reason |
|-------|--------|
| `127.0.0.0/8`, `::1` | Loopback |
| `169.254.0.0/16`, `fe80::/10` | Link-local |
| `10.0.0.0/8` | RFC1918 private |
| `172.16.0.0/12` | RFC1918 private |
| `192.168.0.0/16` | RFC1918 private |
| `169.254.169.254` | Cloud metadata endpoint |

### Egress Policies

Two modes are supported:

- **`DefaultSafe`** — blocks internal IPs, allows all external destinations
- **`StrictAllowlistProxy`** — only allows specific domains via a mandatory proxy

## Credential Scrubbing

**File:** `runtime/src/scrubbing.rs`

All tool output passes through `scrub_tool_output` before reaching the LLM or being stored in memory.

### Keyword-Based Redaction

A compiled `RegexSet` matches key-value patterns:
- `api_key`, `apikey`, `api-key` → value redacted
- `secret`, `password`, `passwd` → value redacted
- `Authorization: Bearer ...` → bearer token redacted
- `token`, `credential` → value redacted

### Entropy-Based Detection

For unlabeled secrets:

1. **Length filter** — string must be 24-512 characters
2. **Character diversity** — must contain a mix of character types (not pure hex, which could be a hash)
3. **Shannon entropy** — must be ≥ 3.8 bits per character

Detected strings are redacted as `[REDACTED:high-entropy]`.

This two-layer approach catches both labeled credentials (environment variable dumps, config files) and unlabeled ones (API keys in tool output, JWTs, base64-encoded secrets).

## Tool Execution Security Flow

The complete security pipeline for a single tool call:

```
1. Tool lookup in registry
2. Safety gate check (ReadOnly/SideEffecting/Privileged)
3. SecurityPolicy enforcement:
   a. For file tools: canonicalize path → check against allowed roots
   b. For shell tools: check syntax → check allowlist
   c. For web tools: validate URL → resolve IPs → check against blocklist
4. Capability profile selection → mount policy enforcement (currently host-based, planned WASM)
5. Timeout enforcement (tokio::time::timeout)
6. Tool execution
7. Output truncation (max 16KB)
8. Credential scrubbing (keyword + entropy)
9. Result returned to runtime
```

Every step is logged via `tracing` spans for auditability.
