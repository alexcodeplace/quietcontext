# QuietContext Token Budget Implementation Plan

> **For agentic workers:** Execute task-by-task with TDD. No subagent dispatch in this run.

**Goal:** Enforce QuietContext six-tool surface and bounded MCP token economics.

**Architecture:** Keep existing executor and FTS5 core. Replace registration interception with explicit public registrations, centralize response budgets, add exact search references, and measure real MCP envelopes.

**Tech Stack:** TypeScript, MCP SDK, Zod, SQLite FTS5, Vitest.

---

## Wave Plan

| Wave | Tasks | Files touched | Safe to parallelize? |
|---|---|---|---|
| 1 | Task 1 | README/config/docs | single task |
| 2 | Task 2 | tests/quietcontext-*.test.ts | single task |
| 3 | Task 3 | src/server.ts, src/store.ts, src/types.ts | single task |
| 4 | Task 4 | package.json, benchmark/test config | single task |
| 5 | Task 5 | generated bundles | single task |

### Task 1: Canonical documentation

**Wave:** 1
**Blocks:** Task 2
**Blocked by:** —

**Files:** Modify `README.md`, `docs/quietcontext-design.md`, `docs/quietcontext-bloat-audit.md`, package/plugin metadata, and Codex config.

**Behavior:** Document Codex-only six-tool contract, short canonical names, token budgets, upstream attribution, and authoritative test gate.

**Acceptance:** `npm run test:quiet -- tests/quietcontext-surface.test.ts` passes documentation/metadata assertions.

### Task 2: Token-budget regression tests

**Wave:** 2
**Blocks:** Task 3
**Blocked by:** Task 1

**Files:** Modify `tests/quietcontext-runtime.test.ts`, `tests/quietcontext-surface.test.ts`; create `tests/quietcontext-token-budget.test.ts`.

**Contract:** Probe built MCP over stdio. Assert exact six names, ≤4 KiB `tools/list`, zero code echo, 8/8/12 KiB response ceilings, fetch metadata-only response, deduplication, and reference retrieval.

**Acceptance:** New assertions fail against pre-fix runtime for expected budget/echo/registry reasons.

### Task 3: Quiet runtime

**Wave:** 3
**Blocks:** Task 4
**Blocked by:** Task 2

**Files:** Modify `src/server.ts`, `src/store.ts`, `src/types.ts`, and focused helpers only.

**Contract:**

- Explicitly register six canonical tools.
- Remove registration monkeypatch and meta-tool exposure.
- Enforce execution/search/batch byte budgets.
- Auto-index oversized execution output.
- Return no fetch preview.
- Deduplicate search chunks across queries.
- Support `search({ refs: string[] })` exact retrieval.

**Acceptance:** `npm run test:quiet` passes.

### Task 4: Economics benchmark and release gate

**Wave:** 4
**Blocks:** Task 5
**Blocked by:** Task 3

**Files:** Modify `package.json`; create benchmark/test configuration as required.

**Behavior:** Make `npm test` authoritative for QuietContext. Benchmark full MCP metadata + argument + response bytes and fail budget regressions.

**Acceptance:** `npm test && npm run benchmark:quiet` exits 0.

### Task 5: Final verification

**Wave:** 5
**Blocks:** —
**Blocked by:** Task 4

**Files:** Regenerate tracked bundles only through existing build.

**Acceptance:** `npm run typecheck && npm run build && npm test && npm run benchmark:quiet` exits 0 without warnings.
