# QuietContext Adaptive Token Controls Implementation Plan

Audience: AI coding agents first.

> **For agentic workers:** REQUIRED SUB-SKILL: use TDD and verification-before-completion. Execute inline; repository dirty state blocks safe worktree creation.

**Goal:** Add opt-in search previews, lower per-call response budgets, and reusable script references without expanding six-tool surface.

**Architecture:** Extend existing schemas and handlers in `src/server.ts`; keep search-schema fields in `src/search/ctx-search-schema.ts`. Use bounded process-local SHA-256 cache for `execute` and `exec-file`. Enforce raw-data indexing before final response caps.

**Tech Stack:** TypeScript, Zod, MCP SDK, Vitest, Codex CLI.

## Wave Plan

| Wave | Tasks | Files touched | Safe to parallelize? |
|---|---|---|---|
| 1 | Task 1 | `src/server.ts`, `src/search/ctx-search-schema.ts`, `tests/quietcontext-token-budget.test.ts`, `tests/quietcontext-economics.ts`, `README.md` | single task |

### Task 1: Adaptive token controls

**Wave:** 1
**Blocks:** —
**Blocked by:** —

**Files:**

- Modify: `src/server.ts`
- Modify: `src/search/ctx-search-schema.ts`
- Modify: `tests/quietcontext-token-budget.test.ts`
- Modify: `tests/quietcontext-economics.ts`
- Modify: `README.md`

- [ ] Add failing MCP tests for title-only search, opt-in preview, `max_bytes`, script reuse, and invalid references.
- [ ] Run targeted Vitest file; confirm failures describe missing fields or old response behavior.
- [ ] Add compact public schema fields without exceeding 4 KiB `tools/list` budget.
- [ ] Add bounded SHA-256 script cache and pre-validation reference expansion.
- [ ] Apply effective response budgets; index execution output exceeding requested budget.
- [ ] Make search previews opt-in and keep exact references retrievable.
- [ ] Update economics benchmark to measure first-call source versus repeated reference arguments.
- [ ] Update README public contract.
- [ ] Run lint, typecheck, build, authoritative tests, economics benchmark, and `git diff --check`.
- [ ] Run isolated Codex with separate `CODEX_HOME`; require live MCP calls covering all three features.
- [ ] Stop before main merge if isolation or verification fails.
