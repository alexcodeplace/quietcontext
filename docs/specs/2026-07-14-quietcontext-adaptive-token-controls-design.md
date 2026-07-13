# QuietContext Adaptive Token Controls

Audience: AI coding agents first.

## Goal

Reduce recurring MCP tokens without adding tools, persisted executable code, or hidden data loss.

## Contract

- `search` MUST return query labels, result titles, sources, and `[r:<id>]` references by default.
- `search(preview: true)` MUST add at most 600 characters per unique result.
- `search(refs: [...])` MUST return referenced content within requested response budget.
- `execute`, `exec-file`, `search`, and `batch` MUST accept optional `max_bytes`.
- `max_bytes` MUST be an integer from 256 through tool hard cap.
- Hard caps remain 8 KiB for `execute`, `exec-file`, and `search`; 12 KiB for `batch`.
- Execution output exceeding effective budget MUST be indexed, never silently truncated.
- `execute` and `exec-file` MUST cache submitted programs of at least 256 bytes by SHA-256-derived reference.
- First cached execution MUST append `[s:<ref>]` to response.
- Later calls MAY replace `language` and `code` with `script_ref`.
- Unknown references and calls containing both `code` and `script_ref` MUST fail closed.
- Script cache MUST remain process-local, hold at most 64 entries, and evict oldest entry first.
- Public MCP surface MUST remain exactly six tools and serialized `tools/list` MUST remain at most 4 KiB.

## Data Flow

1. Registrar receives raw tool arguments.
2. Script resolver validates `code` XOR `script_ref` for execution tools.
3. Resolver expands known reference to cached `language` and `code` before original Zod validation.
4. Handler executes validated arguments.
5. Execution handlers compare raw output against effective `max_bytes`; oversized output enters FTS5.
6. Registrar caps final model-visible text and appends new script reference when applicable.

## Error Handling

- Reject `max_bytes` outside declared range at schema boundary.
- Reject unknown `script_ref` before sandbox execution.
- Reject ambiguous `code` plus `script_ref` input.
- Never persist executable cache across MCP process restart.

## Tests

- Prove default search omits content while `preview: true` restores bounded snippets.
- Prove exact reference retrieval still returns content.
- Prove each variable-output tool honors lower `max_bytes`.
- Prove oversized execution output remains searchable.
- Prove `execute` and `exec-file` reuse long programs through short references.
- Prove invalid and ambiguous script references fail.
- Prove `tools/list` remains within 4 KiB.
- Run real Codex against isolated `CODEX_HOME` and current server bundle before any main merge.

## Architecture Decisions

- Extend existing tools: preserves six-tool contract and avoids another permanent schema payload.
- Use process-local bounded cache: script references are ephemeral capability handles; disk persistence adds security and lifecycle cost without demonstrated need.
- Apply budget after full handler result as final defense, while execution handlers index raw output before capping.
