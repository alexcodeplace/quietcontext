# Shared HTTP daemon — one process serves every session

outcome: quietcontext runs as ONE systemd --user daemon (Streamable HTTP, 127.0.0.1:48619, bearer-token auth) instead of one resident stdio node+bun pair per session; new sessions reach it via the plugin manifest `type: "http"` entry; per-request working root travels in the `X-QuietContext-Root` header (`${PWD}` expansion) and scopes stores/executor via AsyncLocalStorage — no per-session process state.
status: DONE
source request: owner 2026-08-15 — replace ~50 per-session resident stdio MCP children with one shared localhost HTTP daemon; ship end-to-end with before/after numbers.

## Acceptance / receipt (2026-08-15)

- Contract floor: `npm test` 29/29 green (runtime, surface, token-budget, new http suite); `tests/store.test.ts` 131/131 green after trunk merge.
- HTTP suite proves: six-tool surface only, auth rejection (401), root-header validation (400), two concurrent clients with distinct roots interleaved, per-root index/search store isolation.
- Live: `quietcontext-daemon.service` active; two real concurrent `claude -p` sessions served correct per-cwd results by one daemon PID; daemon stopped → tool unavailable, started → works; no new stdio children spawn for new sessions.
- Before/after at 17 idle-but-used sessions: 36 resident processes / 2214 MiB RSS → 1 daemon process / ~80 MiB. Session-start spawn delta: node+bun pair (~130 MiB) → zero processes.
- Rollback: `systemctl --user disable --now quietcontext-daemon` + revert `.claude-plugin/plugin.json` mcpServers entry to `{command:"node", args:["${CLAUDE_PLUGIN_ROOT}/start.mjs"]}` (stdio path and start.mjs remain shipped).

## Constraints honored

- Transport-only HTTP layer (`src/http-server.ts`): routes exclusively through the six public quiet-tool registrations; no legacy/internal wiring. ctx_* legacy tools are stdio-only pending the core-extraction slice.
- Store isolation seam reworked (was per-process, commit 1d8eaef): stores now keyed by resolved working root (`_stores` map in server.ts); stdio child keeps exactly one entry — behavior unchanged there.
- Old sessions keep their stdio children until natural exit; only new sessions use HTTP.

Next executable action: none (follow-up core-extraction slice is a separate plan).
