# QuietContext Token Budget Design

Audience: AI coding agents first.

## Goal

Make QuietContext a six-tool Codex MCP whose own metadata and responses cannot consume material context.

## Scope

- Canonical public tools: `batch`, `execute`, `exec-file`, `fetch-index`, `index`, `search`.
- Compact tool metadata and schemas.
- Hard aggregate response budgets.
- Automatic indexing above direct-output budget.
- Zero source-code echo and zero fetch preview.
- Duplicate-free search output with exact follow-up references.
- End-to-end MCP token-economics benchmark.
- Authoritative QuietContext release gate.

## Approaches

### Approach 1: Dedicated quiet runtime on proven core

Keep executor, security, and FTS5 internals. Remove inherited registration indirection and enforce budgets at six public seams.

| Dimension | Assessment |
|---|---|
| Robustness | Reuses tested data and execution paths; new contracts cover public boundary. |
| Long-term | Quiet surface becomes independent from inherited platform features. |
| Scalability | Existing SQLite and bounded-concurrency behavior retained. |
| Performance | Smaller responses and tool metadata; no extra process. |
| Reversibility | Two-way door; legacy source remains until later pruning. |

**Weakness:** Inherited source remains temporarily, increasing maintenance surface.

### Approach 2: Immediate destructive prune

Delete every non-Codex adapter, lifecycle, analytics, updater, and legacy test before budget work.

| Dimension | Assessment |
|---|---|
| Robustness | Small final surface, but broad deletion obscures token-budget regressions. |
| Long-term | Lowest eventual maintenance burden. |
| Scalability | Equivalent runtime capacity. |
| Performance | Small package/startup improvement beyond Approach 1. |
| Reversibility | One-way door without a clean fork baseline. |

**Weakness:** Large blast radius prevents isolated TDD and reliable review.

**Recommended: Approach 1** — It fixes every model-visible and token-budget defect while preserving proven storage/execution behavior. Remove inherited source only after clean-install and runtime contracts pass.

## Architecture

### Tool registry

`src/server.ts` registers six tools explicitly under canonical public names. Registration filtering, inherited aliases, and meta-tool registration are absent from QuietContext runtime.

### Response budget

One formatter enforces byte ceilings after rendering:

- execution: 8 KiB;
- search: 8 KiB;
- batch: 12 KiB;
- fetch preview: 0 bytes.

Oversized execution output is indexed before response construction. Response returns source label and retrieval instruction; raw bytes remain queryable.

### Search references

Search assigns short process-local references to returned chunks. `search({ refs: [...] })` retrieves exact chunks without another fuzzy query. References expire on server restart.

### Benchmark

Benchmark spawns built MCP server and measures serialized `tools/list`, request arguments, and response text. Payload-only compression numbers are insufficient.

## Data flow

1. Tool handler produces raw output.
2. Raw output stays in executor or FTS5.
3. Formatter deduplicates results and applies tool budget.
4. MCP response returns bounded text plus retrieval references.

## Error handling

- Nonzero execution output uses same 8 KiB ceiling.
- Oversized failures are indexed with error-oriented retrieval metadata.
- Unknown/expired references return compact error listing invalid refs.
- Budget enforcement truncates only rendered snippets, never indexed source content.

## Testing

- Probe real `tools/list`; assert serialized size and exact names.
- Prove source code is absent from `execute` and `exec-file` responses.
- Prove 8 KiB execution auto-index boundary.
- Prove search/batch aggregate ceilings and cross-query deduplication.
- Prove exact reference retrieval.
- Prove fetch responses contain no page preview.
- Run typecheck, build, QuietContext tests, and end-to-end benchmark.

## Architecture decisions

- Dedicated six-tool runtime over destructive full-source pruning. Keeps change reversible while removing inherited surface from execution.
- Short canonical names remain; docs follow runtime. Compatibility aliases rejected because they double tool metadata.
- Process-local references accepted; durable references add storage schema complexity without current need.
- Inherited tests remain non-authoritative until obsolete platform code is pruned.
