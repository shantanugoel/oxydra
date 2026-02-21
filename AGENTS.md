# AGENTS.md

Minimal operating guide for AI agents in this repo.

## Canonical spec

- **Single source of truth:** `docs/guidebook/` (see [guidebook README](docs/guidebook/README.md))
- This file is intentionally lightweight and should not duplicate architecture details.
- If this file and the guidebook diverge, follow the guidebook.

## How to use the guidebook

For every task:
1. Read the relevant chapter(s) in `docs/guidebook/`.
2. Map the task to the progressive build phases in Chapter 15.
3. Make sure you understand how that task may relate to other parts of the system.
4. Then implement the change without trying to over-engineer. Remember ease of maintenance, and readability/contribution by other developers is important.

Quick section map:
- Architecture/layering: Chapters 1-2
- Providers/streaming: Chapter 3
- Tools: Chapter 4
- Runtime: Chapter 5
- Memory: Chapter 6
- Security/sandbox: Chapters 7-8
- Gateway/channels: Chapter 9
- Testing: Chapter 10
- Multi-agent/external channels: Chapters 11-12
- Observability/productization: Chapters 13-14
- Phase order and verification gates: Chapter 15

## Agent execution rules

1. Preserve strict crate boundaries and small-core philosophy.
2. Reuse existing types/traits before adding new abstractions.
3. Keep changes incremental, test-gated, and rewrite-avoiding.
4. Run existing checks for touched areas before finishing.
5. If requirements are unclear or conflicting, ask the user instead of guessing.
6. If you need to arbitrate between different design choices, ask the user (with pros/cons of each approach) instead of guessing.
7. Always make sure to use the latest version of rust crates unless it is not possible due to a specific reason.
8. Any file/function names etc should not be based on plan phase etc, rather behavior based.
9. Whenever any change is made to rust code, make sure the code is errors and warnings free, including clippy, and all tests pass
10. Avoid using clippy allow directives and instead strive to fix them properly.
11. After completing any task, make sure to review if you've achieved what the task was about of was there anything left.
12. Keep any naming for anything in code/filename/tests etc based on behavior and not on the planning phase etc

## Maintenance policy for this file

- Keep `AGENTS.md` short and process-oriented.
- Do not restate chapter-level technical requirements here.
- Update this file only when our core rules change; update `docs/guidebook/` for architecture/design changes.
