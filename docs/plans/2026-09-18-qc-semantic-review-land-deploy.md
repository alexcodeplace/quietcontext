# QC semantic graph review, landing, and local deployment

status: COMPLETE
updated: 2026-09-18

## Goal

Review and improve the semantic repository graph for correctness, CPU, memory, latency, and token efficiency before landing it. Then land the exact reviewed QuietContext build on main, update Overdeck to pin that landed SHA, deploy/converge the workstation through Overdeck, and verify the live local runtime.

## Canonical state

- QuietContext worktree: `/home/user/Projects/.worktrees/quietcontext-qc-public-20260910`
- branch: `feat/qc-semantic-graph-20260917`
- pre-review semantic commits: `0e7c02b3` and `fe251054`
- accepted review/runtime commit: `7500ae2affe914c74471681da1cd2372663ee069`
- QuietContext integration landed at `dd0d9aee848b350c4ecae61e4f74368ddc9dab48`; current public main still contains that commit
- keep MCP graph actions under the existing compact `repo` tool
- keep structural navigation cheap; semantic work is demand-driven

## Review improvements already implemented

- Tree-sitter nesting is used directly instead of repeated full symbol scans.
- The quadratic symbol-ownership pass is removed.
- Bare-name repository-wide guessing is removed.
- Resolution now prefers lexical/same-file scope, explicit imports/aliases, receiver/type ownership, and language-valid package scope; otherwise it remains unresolved.
- Go same-package cross-file calls remain supported.
- Repeated file-path and traversal strings were compacted into file-node identity.
- Raw references store byte spans rather than allocating a name string per occurrence.
- `contains` edges are excluded from traversal adjacency maps.
- Semantic graph materialization is lazy via `OnceLock`.
- Per-file unresolved semantic facts are cached and shared across immutable generations for unchanged files.
- Semantic client startup/request budgets are longer than structural budgets so a cold semantic query does not race a second direct full build against the daemon.

## Measured review evidence

- original direct semantic query on QC repo: about 16.1 s
- reviewed semantic cold path after fact reuse: about 6.2 s
- warm semantic query: about 11 ms
- pre-review one-file semantic refresh: about 5.5 s
- reviewed one-file semantic refresh: about 1.8-1.9 s
- structural-only daemon after `qc map`: roughly 23-32 MiB RSS
- semantic daemon after cold graph build: roughly 105 MiB RSS
- semantic daemon after edit/restore cycles: roughly 115 MiB RSS
- pre-review edit/restore cycles reached roughly 155-158 MiB RSS

## Remaining tasks

### R1 Global daemon memory budget after lazy semantic growth [DONE]

- Recompute total logical bytes after a semantic graph/facts materialize.
- Evict inactive least-recently-used roots when lazy growth would exceed the daemon-wide logical byte budget.
- If the active root alone cannot fit, fail closed.
- Preserve the existing RSS hard stop.

### R2 Regression tests [DONE]

Add tests proving:

1. structural generation exists without semantic materialization
2. first semantic access materializes once
3. unchanged SourceRecords share semantic facts across refresh
4. changed files get fresh semantic facts
5. semantic refresh observes changed edges/definitions
6. logical byte accounting grows when facts/graph materialize
7. daemon-wide accounting handles lazy growth without exceeding the configured cap

### R3 Spec update [DONE]

Update `docs/specs/2026-09-17-qc-semantic-repository-graph.md` to document:

- no repository-wide unique-name fallback
- language-valid package scope
- lazy semantic materialization
- per-file semantic-fact reuse
- structural vs semantic client budget split
- reviewed memory/accounting behavior
- final acceptance evidence

### R4 Final cleanup and acceptance [DONE]

- remove new compiler warnings
- `git diff --check`
- forbidden-name audit clean
- Rust suite
- release Rust build
- native staging
- TypeScript/build/bundle gates
- package tests
- native corpus/CLI tests
- HTTP/MCP integration
- `tools/list` <= 4 KiB for stdio and HTTP
- watcher edit/delete semantic freshness
- collision/file-filter behavior
- final benchmark smoke and artifact hashes

## Progress receipts

- 2026-09-18: R1 implemented. Lazy semantic growth is re-admitted against the daemon-wide logical byte cap and evicts inactive LRU roots as required.
- 2026-09-18: R2 implemented. Added lazy materialization, unchanged-fact reuse/changed-fact reset, dynamic byte accounting, and multi-root lazy-growth eviction regression tests.
- 2026-09-18: focused Rust gate is 118/118 green on the reviewed dirty worktree.
- 2026-09-18: R3 implemented. Semantic spec moved to `ACTIVE_REVIEW` and now documents lazy semantics, per-file fact reuse, strict lexical/import/package resolution, split client budgets, and lazy-growth memory accounting.
- 2026-09-18: R4 exact-commit acceptance passed on clean commit `7500ae2affe914c74471681da1cd2372663ee069`.
- authoritative K3s job: `overdeck-build-build-20260918050211-1356483-19280`, node `debian4`, streamed source SHA-256 `f1f719026b703e9729a7f868662160819cc334ca4262673e05fd69b998bc27cc`
- Rust: 118/118 passed
- native release staged/verified: `linux-x64`, native `0.1.0`, SHA-256 `10964067a98d9bc243e016b677ccdb3418d2cbfcc0db226da0a26f8d0cd66553`
- production TypeScript/bundle/assert-bundle/asymmetric-drift: green
- package suite: 14 files, 91 passed, 1 expected platform skip
- token budget: `tools/list` <= 4 KiB over stdio and HTTP and byte-identical
- native integration/corpus/HTTP/fidelity: 4 files, 18 passed, 1 platform-specific skip
- exact-commit benchmark: structural cold map 0.2086 s; structural warm map 0.1334 s; semantic cold 5.8693 s; semantic warm 0.0029 s; one-file semantic refresh 1.7473 s
- exact-commit acceptance verdict: `ACCEPTANCE_OK`
- R4 is complete. QuietContext landing, Overdeck pinning, and workstation deployment are complete.

## Final landing and deployment receipt

Completed 2026-09-18.

QuietContext landing:

- reviewed runtime commit: `7500ae2affe914c74471681da1cd2372663ee069`
- public integration commit deployed/pinned by Overdeck: `dd0d9aee848b350c4ecae61e4f74368ddc9dab48`
- later public-main head observed during closeout: `ecbad18add26e3f0021b2a9fb9b1f24b2454f23a`; `dd0d9aee...` is verified as its ancestor
- package/native/protocol: `1.1.0-rc.3` / `0.1.0` / `2`
- installed Linux x64 native SHA-256: `10964067a98d9bc243e016b677ccdb3418d2cbfcc0db226da0a26f8d0cd66553`

Overdeck landing:

- reviewed QC pin landed through the serialized Overdeck land queue
- landed candidate: `d4b2540957d3276368b9be49a69adf58a507ef12`
- land ticket: `ticket.4383d43f3116412e8e505397c6274fc7`
- land receipt: `/home/user/Projects/overdeck/.git/harness/land-receipts/f097bf77bafb-hxmj61tr`
- later Overdeck main observed during closeout: `a6ba56093bbb3165c7ab35c7742138e90843c549`; `d4b254...` is verified as its ancestor
- the landing lane also repaired two unrelated but real trunk-wide frozen-gate regressions encountered during serialization: the Longhorn fixture now pins its sandbox under `/tmp`, and the Debian1 portability test allows only four exact migration/compatibility seams while proving each migrates/refuses the old authority

Workstation rollback and convergence:

- fresh pre-mutation rollback snapshot: `~/.local/state/overdeck/qc-cutover-backup/20260918T063305Z-qc-semantic-pre`
- managed checkout: `~/.claude/plugins/sources/quietmode`
- checkout HEAD and Overdeck build stamp: `dd0d9aee848b350c4ecae61e4f74368ddc9dab48`
- `~/.local/bin/qc` and `~/.local/bin/ft` resolve to the same managed `bin/qc.mjs`
- `quietcontext` and `context-mode` resolve to the managed `cli.bundle.mjs`
- local dependency/build constraints were handled without local Rust compilation: the exact native and compiled QC outputs were produced in the sanctioned K3s builder and published into the managed checkout after hash/protocol validation
- `quietcontext-daemon.service` is enabled and active from the managed checkout; `/healthz` returned QuietContext `1.1.0-rc.3`
- QC Bash routing remains disabled and the routing marker remains absent

Live canaries:

- `qc status`: QuietContext `1.1.0-rc.3`, native `0.1.0`, protocol `2`
- live `qc callers build_map` and `qc impact build_map --depth 2` returned semantic edges from the installed repository
- live shared HTTP MCP `tools/list`: 7 tools, names `repo,execute,exec-file,index,search,fetch-index,batch`, serialized size 4066 bytes
- live authenticated HTTP MCP `repo` action `callers` for `build_map` returned the expected semantic caller result
- Claude settings were patched only with the QC-specific target delta: local QuietContext marketplace, `quietcontext@quietcontext=true`, legacy context-mode disabled, and the QC Bash hook hard-dark with `QUIET_CONTEXT_QC_BASH_ROUTING=0`; unrelated live Slopgate/Vibebotmaster settings were preserved
- observed resident processes after canaries: one shared HTTP daemon and one native repository daemon; no per-session QC process fanout

Definition-of-done result: all acceptance, landing, pinning, local convergence, semantic CLI/MCP, daemon, rollback, and routing-dark requirements are satisfied.

## QuietContext landing sequence [DONE]

1. Finish R1-R4.
2. Record final evidence in this plan and the semantic spec.
3. Commit review/performance improvements separately on the feature branch.
4. Verify a clean worktree.
5. Fetch and inspect current public `origin/main`.
6. Integrate the semantic chain onto current main without dropping unrelated upstream changes.
7. Rerun exact-commit acceptance if the integrated tree differs from the tested tree.
8. Push QuietContext main.
9. Verify remote main contains the reviewed semantic implementation.

## Overdeck deployment sequence [DONE]

1. Fetch current Overdeck remote state.
2. Create an isolated Overdeck worktree from current remote main.
3. Inspect the current QC cutover/deployment contract and current devtool pin.
4. Pin `modules/buildbox/devtools.json` to the exact landed QuietContext SHA.
5. Update only genuinely required protocol/version expectations.
6. Run affected Overdeck QC/buildbox/workstation gates.
7. Commit and land the Overdeck pin through the canonical workflow.
8. Converge/deploy the workstation through Overdeck.
9. Verify live local `qc`, daemon/service state, semantic CLI commands, MCP repo action, and the compatibility alias contract.

## Definition of done

This lane is complete only when:

- all reviewed QC gates are green
- review improvements are committed
- QuietContext remote main contains the reviewed implementation
- Overdeck main pins that exact landed SHA
- Overdeck affected gates are green
- the workstation is converged through Overdeck
- local `qc` and MCP semantic queries are verified against the landed SHA
- daemon memory/accounting invariants hold
- this plan and the semantic spec contain final evidence
- relevant worktrees are clean

All definition-of-done items above were satisfied on 2026-09-18; status is `COMPLETE`.
