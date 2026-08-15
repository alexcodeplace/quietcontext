# QuietContext Design

Audience: AI coding agents and maintainers of the Codex-focused fork.

## Goal

Keep sandboxed processing, indexing, and search while removing plugin-generated routing, memory, and lifecycle narration from model-visible context.

## Product contract

- Expose exactly `batch`, `execute`, `exec-file`, `fetch-index`, `index`, and `search`.
- NEVER expose inherited `ctx_*` aliases or meta-tools.
- NEVER inject routing, memory, lifecycle, analytics, or update narration.
- NEVER echo submitted code or raw fetched content.
- Enforce aggregate response budgets before returning MCP content.
- Return short search references for exact follow-up retrieval.
- Leave context compaction to Codex; the plugin no longer injects resume snapshots or memory hints.
- Preserve full raw data in sandbox or FTS5; response limits MUST NOT discard retrievable data.
- Preserve launcher self-healing for installed-plugin/cache repair.
- Validate through isolated storage and the authoritative QuietContext test gate.

## Non-goals

Production Codex configuration remains user-owned. Non-Codex adapters and inherited analytics may remain in source temporarily, but MUST NOT enter QuietContext runtime, package surface, docs, or release gate.

## Acceptance

- Serialized `tools/list` ≤4 KiB.
- Direct execution responses ≤8 KiB.
- Search responses ≤8 KiB; batch responses ≤12 KiB.
- Fetch indexing returns metadata only.
- Duplicate search chunks appear once per response.
- `npm test`, `npm run typecheck`, and `npm run benchmark:quiet` exit 0 without warnings.
