# Bound QuietContext indexing CPU and storage

status: DONE
owner: main session
task IDs: overdeck incident #2
source request: owner reported recurring workstation CPU saturation; measured QuietContext processes consume 174% CPU, including one attached-session process at 91%.

## Outcome

Oversized tool output cannot create unbounded FTS work, disk amplification, or sustained idle CPU burn.

## Acceptance criteria

1. Every stored content chunk respects configured byte cap; titles and total chunk count have independent hard bounds. Oversized Markdown paragraphs, code blocks, lines, headings, JSON values, and compact arrays are covered.
2. Index admission has deterministic input-byte, read-byte, parsed-work, and output-chunk bounds before trigram insertion and vocabulary extraction.
3. FTS maintenance cannot synchronously pin an MCP process on oversized accumulated state.
4. Regressions reproduce oversized Markdown and total-input cases and prove bounded storage/work.
5. Build and focused tests pass without warnings.
6. Install candidate into actual QuietContext source, prove new process behavior, then commit, land, reinstall landed source, and repeat proof.
7. Preserve attached sessions. Never kill or restart an attached Claude session to remove an old plugin child.

## Preserved WIP

- Worktree: `/home/user/Projects/quietmode/.worktrees/cpu-burn`
- Base snapshot: `81b1d43`
- Installed source: `/home/user/.claude/plugins/sources/quietmode`
- Hot attached process: PID 2315072 in `tmux-spawn-f8235b88-9e3c-4878-b51b-b5e9b1c4e30a.scope`; do not terminate.

## Constraints

- Source fix belongs in QuietContext, not an Overdeck runtime patch.
- Keep large outputs searchable only within explicit bounded admission; reject rather than silently index partial misleading content.
- Do not weaken filesystem or command deny policies.

## Execution steps

1. Add failing storage tests for oversized Markdown and total admission.
2. Implement byte-capped chunking and input admission.
3. Replace synchronous full-index optimization with bounded maintenance or remove it when measurement shows it causes the spin.
4. Run focused tests and build.
5. Install and prove actual runtime, land, reinstall, re-prove.

## Current receipt

- Instantaneous sample: 46 QuietContext wrapper/worker processes consumed 173.7% CPU.
- PID 2315072 alone consumed 91%; it has read 63 GB and written 36 GB over 33 h.
- Its DB is 1.03 GB plus 540 MB WAL. Two sources contain about 105 MB each; one has a single 104.7 MB chunk, violating the nominal chunk cap.
- Trigram index storage is 618 MB. Source insertion runs synchronous FTS `optimize` every 50 inserts and on close.
- Worktree implementation caps every source at 8 MiB before chunking/JSON parsing, preflights file size before read, byte-splits oversized Markdown/JSON chunks, and removes synchronous full-index `optimize` from insertion and close paths.
- Remote focused regressions passed: 3 passed, 121 skipped. Full store suite passed: 124 passed.
- Remote dependency install reported ignored `better-sqlite3`/`esbuild` build scripts; explicit remote rebuild completed exit 0, and SQLite-backed tests then passed.
- Remote TypeScript/build/bundle assertions passed. Nested `npm run` emitted four environment-key warnings inherited from the pnpm launcher; these are existing build-runner configuration noise, not candidate diagnostics. No compiler, bundle, or source warning occurred.
- First independent review found five real boundary defects: repeated oversized titles, quadratic JSON-array batching, growth-after-`fstat` reads, sub-code-point byte caps, and removal of a public static member. Fixes add title/chunk-count budgets, cached per-item JSON serialization/byte accounting, descriptor reads capped at 8 MiB + 1 byte, actual-code-point fit checks, and compatibility member preservation.
- Runtime accounting independently read a file before the store preflight. It now uses `statSync().size`, leaving the store's descriptor-bound capped read as the only content read.
- Second review found blank-section budget aggregation, unbalanced oversized code fences, and over-restrictive small byte caps. Fixes enforce one source-wide budget before mutation, rewrap byte-budgeted code fragments with balanced fences, and accept every positive integer cap when the actual code points fit.
- Third review found two further resource defects: repeated byte-length scans made long-line splitting quadratic, and every file read allocated the full 8 MiB allowance. Fixes now scan each code point once and emit bounded slices incrementally, while descriptor reads reuse a 4–64 KiB buffer and accumulate no more than 8 MiB + 1 byte.
- Final store suite passes 131/131; the 2 MiB linear-split regression completes in 131 ms. Final TypeScript/build/all bundle assertions pass with no warnings.
- Fourth independent adversarial review found no remaining defect in `src/store.ts`, `src/server.ts`, or `tests/store.test.ts`.
- Generated bundles were pulled back; only `server.bundle.mjs` is runtime-relevant to this source change, and unrelated generated-artifact drift was restored before installation/commit.

- Candidate installed atomically into the actual QuietContext source with exact rollback at `/home/user/.local/state/quietcontext/backups/cpu-burn-20260811T163327Z`; no existing plugin process or attached session was restarted or signalled.
- Actual installed `start.mjs` proof rejected 8 MiB + 1 byte in 1.446 s, then indexed and searched normal content, and exited cleanly: `installed-entrypoint-bounds: ok`.

- Candidate commit was rebased as `694821f4a4ef49b6495483a796e50fe7e03c833d` onto current `quietcontext` history, the exact proven source/bundle hashes were preserved, and rebased store tests passed 131/131.
- `origin/quietcontext` advanced by fast-forward to `694821f4`; the landed artifact was reinstalled atomically with rollback at `/home/user/.local/state/quietcontext/backups/pre-landed-20260811T163700Z`.
- Landed actual-entrypoint proof again rejected 8 MiB + 1 byte, indexed/searched normal content, and exited cleanly: `landed-installed-entrypoint-bounds: ok` (0.412 s rejection).

## Next executable action

None. Task delivered and installed; the parent Overdeck incident continues with its separate kill-guard bypass task.
