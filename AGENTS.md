# AGENTS.md

Minimal operating guide for AI agents in this repo.

## Canonical spec

- **Single source of truth:** `oxydra-project-brief.md`
- This file is intentionally lightweight and should not duplicate architecture details.
- If this file and the research document diverge, follow `oxydra-project-brief.md`.

## How to use the research document

For every task:
1. Read the relevant chapter(s) in `oxydra-project-brief.md`.
2. Map the task to the progressive build phases in the Appendix.
3. Make sure you understand how that task may related to other parts of the system.
4. Then implement the change without trying to over-engineer. Remember ease of maintenance, and readability/contribution by other developers is important.

Quick section map:
- Architecture/layering: Chapter 1 + Appendix workspace layout
- Providers/runtime/streaming/tools: Chapters 2-5
- Memory/security: Chapters 6-7
- Gateway/multi-agent routing: Chapter 8
- Phase order and verification gates: Appendix progressive build plan

## Agent execution rules

1. Preserve strict crate boundaries and small-core philosophy.
2. Reuse existing types/traits before adding new abstractions.
3. Keep changes incremental, test-gated, and rewrite-avoiding.
4. Run existing checks for touched areas before finishing.
5. If requirements are unclear or conflicting, ask the user instead of guessing.
6. If you need to arbitrate between different design choices, ask the user (with pros/cons of each approach) instead of guessing.
7. Always make sure to use the latest version of rust crates unless it is not possible due to a specific reason.

## Maintenance policy for this file

- Keep `AGENTS.md` short and process-oriented.
- Do not restate chapter-level technical requirements here.
- Update this file only when our core rules change; update `oxydra-project-brief.md` for architecture/design changes.
