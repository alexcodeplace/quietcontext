# Legacy tool registration: dormant on every server.ts transport

Audience: AI coding agents and maintainers touching `src/server.ts` or the
OpenClaw adapter.

`registerLegacyTools()` and `registerLegacyInsightTool()` in `src/server.ts`
define `ctx_stats`, `ctx_doctor`, `ctx_upgrade`, `ctx_purge`, and `ctx_insight`
via `server.registerTool(...)`, but neither function is called anywhere in
`src/server.ts`. This is intentional (owner ruling, core-extraction carve
lane, 2026-08): the bodies stay as a **dormant boundary** — retained upstream
maintenance tools kept dead-but-present so upstream rebases stay easy without
polluting `tools/list` on any of the three live server.ts transports (stdio
boot, HTTP daemon, OpenCode in-process). Do not wire them up as a "fix" for
apparent dead code; do not delete them either.

**Asymmetry:** OpenClaw does not go through this dormant path at all. Its own
`src/adapters/openclaw/mcp-tools.ts` registers the same five `ctx_*` names
independently and routes each through `cliRedirect(toolName)` into
`src/cli.ts`'s command handlers (e.g. `cli.ts`'s `insight()` for `ctx_insight`).
So OpenClaw users get working `ctx_stats`/`ctx_insight`/etc. today, while
every other client sees none of these five tools — two independent
implementations of the same tool names, only one of which is reachable. This
is a documented asymmetry, not a bug to reconcile in this slice.

## Correction: `insight/` (the directory) is NOT an orphan

Do not confuse the dormant `ctx_insight` *tool* above with the `insight/`
*directory* at repo root (a standalone Vite/React dashboard + `server.mjs`).
The core-extraction carve lane's task brief listed `insight/` alongside
`web/` as an "inventory-proven orphan, zero inbound references" slated for
deletion. That is false: `tests/analytics/insight-cors.test.ts` spawns
`insight/server.mjs` directly and asserts its CORS behavior against real
fixture SQLite DBs. Deleting `insight/` breaks that test (verified: full
`vitest run` sweep goes from 174 pre-existing failures to 176 with `insight/`
removed, and the new failure is exactly this test). `insight/` was restored;
only `web/` (a single unreferenced `index.html`, confirmed via import-graph
and string-literal grep, and not present in `package.json`'s `files[]`) was
actually deleted in this lane.
