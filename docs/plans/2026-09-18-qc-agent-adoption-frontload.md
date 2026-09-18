# QC agent adoption front-load

status: ACTIVE
updated: 2026-09-18

## Goal

Make coding agents reliably use QC for structural/code-understanding work without relying on memory or coercive Read/Grep blocking.

Adopt the proven external graph-tool pattern:

1. front-load structural prompts at UserPromptSubmit time
2. expose one obvious primary exploration behavior
3. keep that primary MCP path visible/always-loaded where the client supports it
4. make returned context sufficient enough that exploratory Read/Grep is unnecessary
5. measure adoption instead of assuming instructions work

## Current verified state

- QuietContext public main: `bf52c59b80ec0c3f9a4757671ad77300d9f0312a`
- worktree: `/home/user/Projects/.worktrees/quietcontext-qc-adoption-20260918`
- branch: `feat/qc-agent-adoption-20260918`
- installed workstation QC remains pinned to the previously accepted runtime `dd0d9aee...`; do not mutate the live install until this lane is acceptance-gated and landed
- QC already has a shared `hooks/userpromptsubmit.mjs` used by Claude-like adapters, currently for session continuity only
- QC MCP currently exposes 7 public tools and keeps graph operations inside the existing `repo` tool
- Overdeck currently has its own UserPromptSubmit hooks for transcript journaling and botmaster inbox; QC front-load must be additive and must not remove them
- automatic QC Bash routing remains a separate feature and stays disabled

## External pattern being copied

the external graph tool's current adoption strategy uses these reinforcing layers:

- Claude UserPromptSubmit prompt hook front-loads graph context for structural prompts
- one primary `external graph tool_explore` tool is Read-equivalent and intentionally sufficient
- MCP initialize guidance tells the agent to use the graph instead of rebuilding it with Read/Grep
- Claude marks the primary tool always-loaded so it is visible before tool search
- broad Read/Grep denial was rejected because agents route around it and waste calls
- prompt-hook output is capped below Claude's inline hook-output threshold
- prompt classification is multilingual / symbol-aware and fail-open

QC should copy the architecture, not the external graph tool's exact classifier or payload size.

## Slice A: primary explore behavior

Add `repo action=explore` and CLI `qc repo explore <query>` / `qc explore <query>`.

Contract:

- accept a natural-language question or symbol/file bag
- return compact source-adjacent context sufficient for structural reasoning
- include matched symbols/files, relevant graph relationships, and bounded blast radius
- prefer named symbols/file hints from the query
- cap output deterministically
- do not add another public MCP tool
- preserve existing repo actions

Initial implementation may compose existing symbol/map/callers/callees/impact primitives rather than invent a second graph engine.

## Slice B: UserPromptSubmit semantic front-load

Extend the existing shared UserPromptSubmit hook.

For a genuine user prompt:

1. preserve current session journaling behavior
2. classify whether the prompt is structural/code-exploration work
3. resolve the hook project root only from the supplied project/cwd, never by scanning unrelated parent/home projects
4. for eligible prompts, call the local QC explore path with a strict time/output budget
5. emit Claude-compatible `hookSpecificOutput.additionalContext` containing a compact tagged QC block
6. non-structural prompts, unsupported roots, timeouts, stale/unavailable QC, or any error inject nothing and exit 0

Safety/performance:

- fail open
- no prompt blocking
- no Read/Grep denial
- no unbounded filesystem down-scan
- no home/root adoption
- max injected payload <= 8.5 KiB, leaving wrapper headroom under Claude's inline threshold
- bounded hook wall-clock budget
- no new daemon per prompt; reuse existing repo daemon/shared HTTP/native path where applicable

Classifier requirements:

- deterministic and cheap
- structural intent signals plus code-shaped/symbol signals
- multilingual-friendly: do not require English-only keywords
- obvious non-code/chitchat prompts should remain no-op
- explicit code file/symbol names can trigger even without structural keywords

## Slice C: MCP guidance + visibility

- add compact MCP server guidance telling agents:
  - use `repo explore` first for structural questions and before exploratory Read/Grep
  - treat returned source/context as already inspected
  - do not re-verify with grep unless the result is stale/incomplete
  - after edits, use focused reads/tests as needed
- mark the existing `repo` tool with Anthropic always-load metadata if supported by the current SDK/schema without breaking strict clients
- preserve the 4 KiB public tools/list budget; if always-load metadata would violate/surface incompatibly, keep it client-config-only in Overdeck instead

## Slice D: adoption telemetry

Record bounded local telemetry sufficient to answer:

- structural prompts seen
- prompts front-loaded
- front-load no-op reasons
- front-load latency
- explore output bytes
- subsequent QC repo calls
- subsequent raw Read/Grep usage when observable through existing session hooks

Do not log full user prompts into a new telemetry stream. Reuse existing session records/feature extraction where possible and store counters/reasons only.

## Overdeck rollout

After QC acceptance and public-main landing:

- update the exact QC pin
- add QC's UserPromptSubmit hook alongside existing live-transcript/botmaster hooks
- do not remove existing hooks
- keep QC Bash routing hard-dark
- update cutover/adoption tests to prove:
  - existing UserPromptSubmit hooks remain
  - QC UserPromptSubmit hook is additive
  - no Read/Grep deny behavior was introduced
  - exact QC pin and plugin path are correct

## Acceptance

QC:

- unit tests for classifier: structural, non-structural, multilingual/code-shaped, unsafe cwd/root, timeout/error fail-open
- hook tests prove current session capture still works
- front-load hook returns valid UserPromptSubmit additionalContext
- context <= 8.5 KiB
- `repo explore` / CLI explore deterministic and bounded
- existing callers/callees/impact/path behavior unchanged
- MCP tools/list stays <= 4 KiB and public tool count remains 7
- package build/test green
- native tests green where changed

Behavioral canary:

- structural prompt receives QC context before agent reasoning
- non-structural prompt injects zero QC context
- structural prompt can be answered from injected context without exploratory Read/Grep in a controlled agent-eval scenario
- hook failure leaves prompt execution unaffected

Overdeck:

- QC pin landed through serialized land queue
- current UserPromptSubmit journaling/botmaster hooks preserved
- QC front-load hook added
- local deployment converged
- routing remains disabled
- fresh session/client canary sees QC front-load and MCP repo path


## Self-review checkpoint — token efficiency and performance

status: PERFORMANCE_REVIEW_ACCEPTED

The adoption architecture remains correct, but the first implementation pass is not efficient enough to land unchanged.

### Findings

1. **Prompt front-load currently creates too many process hops.**
   - UserPromptSubmit runs a Node hook.
   - Eligible prompts synchronously spawn another Node process for `qc repo explore`.
   - `explore` currently composes native repo calls as separate subprocesses:
     - 1 map
     - up to 3 symbol lookups
     - 1 callers + 1 callees + 1 impact for the strongest symbol
   - worst case today: 1 extra Node process + up to 7 native QC child processes for one user prompt.
   - This is acceptable for an occasional explicit CLI call, but too expensive for an automatic prompt hook.

2. **Explore duplicates graph work.**
   - `callers`, `callees`, and `impact` are requested separately even though they share the same root/symbol and semantic graph generation.
   - The native daemon makes each individual query cheap once warm, but client/process overhead and repeated request framing remain.
   - A dedicated single native `explore` operation, or one batched daemon request, would be materially better.

3. **The front-load hook is synchronous.**
   - `spawnSync` blocks UserPromptSubmit until exploration finishes.
   - The current default timeout is 7.5 s, which is too high for an automatic hook.
   - The automatic path should have a much tighter latency budget than explicit `qc explore`.
   - Target: warm front-load p95 < 300 ms, fail-open by ~500–750 ms; explicit CLI/MCP explore may use a larger budget.

4. **The automatic payload cap is too large.**
   - Current injected cap is 8.5 KiB.
   - This is safe relative to hook transport limits but not ideal for token economy.
   - 8.5 KiB can be roughly 2k+ tokens before the agent has reasoned.
   - Better default target: 2.5–4 KiB, with explicit `repo explore` allowed a larger answer.
   - The hook should inject only the top symbol, concise source window, and highest-value relationships.

5. **Source excerpts are currently wider than necessary.**
   - Current source radius is ±8 lines per matched symbol.
   - Up to 3 candidates can produce source excerpts even though only the strongest candidate receives graph fan-out.
   - Automatic front-load should usually include source for only the strongest candidate; secondary candidates should be names/locations only.

6. **Telemetry performs file I/O on every prompt.**
   - Even non-structural prompts currently append a JSONL event.
   - That means every UserPromptSubmit can mkdir/stat/append on the hot path.
   - This is unnecessary overhead and creates write amplification.
   - Better: record only eligible/front-loaded/error samples, aggregate counters in existing session/event telemetry, or sample no-op outcomes.

7. **Classifier is conceptually cheap, but the current working copy is malformed and must be repaired before any further acceptance.**
   - A bad edit corrupted `matchesStructuralTerm`.
   - The most recent gate correctly failed front-load tests because of that syntax damage.
   - This is a worktree defect, not a flaw in the architecture, but it reinforces the need for one final clean gate after the performance rewrite.

8. **Natural-language candidate ranking from the full textual map is workable but not the ideal hot path.**
   - Pulling/parsing the full map for every eligible prompt adds avoidable bytes and ranking work.
   - Better native seam: a bounded symbol-candidate query returning top-N candidates directly.
   - This avoids serializing thousands of declaration names through Node on every structural prompt.

9. **The always-load + MCP-initialize guidance is efficient and should stay.**
   - It adds negligible runtime cost.
   - The public tool count remains 7.
   - The compact public repo schema saves schema bytes.
   - This is the strongest low-cost adoption lever besides front-load.

10. **Do not add a permanent global CLAUDE.md rule.**
    - QC now has stronger mechanical levers.
    - A permanent preamble rule costs tokens every session/compaction and duplicates behavior already encoded in MCP metadata + prompt front-load.
    - Keep the owner-controlled global preamble lean.

11. **Native artifact verification is repeated per repo call.**
    - `resolveQcNativeArtifact()` currently reads the manifest/package and SHA-256 hashes the native binary on every `repoQcNative` invocation.
    - The first explore draft can invoke `repoQcNative` up to seven times, so one automatic prompt may hash the same binary up to seven times.
    - A one-request native explore path removes most of this immediately; separately, artifact verification should be safely memoized for the lifetime of the Node process using stable file identity/mtime/size where appropriate.

12. **Measured CLI/process overhead confirms the hot-path concern.**
    - On the current installed QC, a timed `qc repo map` call against the adoption worktree took about 3.6 s and `qc repo symbol build_map` about 2.6 s even after a map warm-up.
    - The first semantic `callers build_map` on that root exceeded a 20 s measurement timeout while semantic indexing was cold.
    - These numbers are not acceptable for a synchronous UserPromptSubmit hook and validate the tighter design target below.
    - The automatic front-load path should therefore bypass the human-facing `qc` CLI process and talk to the already-running shared daemon/native service through one bounded request.

### Revised implementation direction

Keep:

- existing `repo` tool, no new public MCP tool
- `action=explore`
- MCP initialize guidance
- `anthropic/alwaysLoad` metadata on `repo`
- fail-open UserPromptSubmit classifier
- no Read/Grep blocking
- multilingual/code-shaped triggering
- routing remains independent/dark

Change before landing:

1. replace map-text ranking with a bounded native candidate lookup or equivalent compact daemon query
2. make native explore one request, not 4–7 child-process requests
3. automatic hook injects only one primary source excerpt + compact relationships
4. reduce automatic context target to 2.5–4 KiB
5. automatic hook timeout target 500–750 ms; explicit explore keeps a larger timeout
6. remove non-structural per-prompt JSONL writes; use aggregated/sampled telemetry
7. benchmark:
   - no-op prompt hook
   - warm structural front-load
   - cold structural front-load
   - injected bytes/tokens
   - native child-process count
   - daemon RSS/CPU delta
8. only land after clean build/tests and live behavioral canary

### Performance acceptance targets

- non-structural hook overhead: <= 10 ms p95 excluding existing session DB work
- eligible warm front-load: <= 300 ms p50 and <= 500 ms p95 where repo daemon is warm
- hard fail-open deadline: <= 750 ms for automatic injection
- automatic injected context: target <= 3.5 KiB; hard cap <= 4 KiB
- one native/daemon explore request per eligible prompt
- zero extra persistent processes
- public MCP tools/list remains <= 4 KiB and tool count remains 7
- explicit `repo explore` may return up to 8 KiB because it is agent-requested rather than automatically injected


## Optimized rewrite acceptance checkpoint

The rejected multi-process draft was replaced before landing.

Current architecture:

- UserPromptSubmit performs only a cheap local classifier for non-structural prompts.
- Eligible prompts make one authenticated request to the already-running shared QC HTTP daemon.
- The MCP `repo action=explore` handler makes one native QC request.
- Native `Explore` ranks candidates directly from `SourceIndex`; it does not serialize/parse the full textual map through Node.
- Cold explore is structural-only and deliberately does not materialize the semantic graph.
- If that generation's semantic graph is already warm, explore reuses it for compact callers/callees/impact without rebuilding it.
- Automatic context hard cap is 4 KiB.
- Automatic hook default deadline is 650 ms.
- Native explore gets a 500 ms daemon request/start budget, preventing the 25 ms ordinary-structural duplicate-scan race.
- Non-structural prompts perform no telemetry write and no daemon request.
- Telemetry records only eligible outcomes (injected/no-match/timeout/error), never the prompt body.
- No Read/Grep denial was added.

Exact feature SHA acceptance before final docs update:

- feature commit: `74b8a01f855624c7d407f44914ff52d0d5843a2f`
- exact K3s acceptance job: `overdeck-build-build-20260918124230-2971463-16534`, node `debian1`
- Rust: 121/121 passed
- production TypeScript/bundle/assert-bundle/asymmetric-drift: green
- focused adoption/session/token tests: 86/86 passed
- MCP public surface remains 7 tools; stdio/HTTP tools/list remain byte-identical and <= 4 KiB
- `repo` carries `anthropic/alwaysLoad` metadata through the shared tools/list compatibility wrapper
- MCP initialize guidance tells agents to use `action=explore` before exploratory Read/Grep

Production-shaped benchmark job:

- job: `overdeck-build-build-20260918124635-2999403-28584`, node `debian1`
- build: exact GitHub SHA `74b8a01f...`, release native staged through the normal vendor path, production shared HTTP daemon
- non-structural hook, n=50: average 0.2 ms, p50 0.1 ms, p95 0.5 ms, max 2.9 ms, 0 injected bytes
- cold structural first request: 655.9 ms, fail-open, 0 injected bytes
- warm structural hook, n=20: average 59.9 ms, p50 34.6 ms, p95 41.3 ms, one startup outlier 530.6 ms
- warm automatic payload: average 1,099 bytes
- direct native explore, n=20: average 5.4 ms, p50 5.4 ms, p95 5.5 ms
- benchmark verdict: `QC_ADOPTION_PERF_OK`

Performance review verdict:

- no-op overhead is far below the <=10 ms target
- warm p95 is far below the <=500 ms target
- cold path fails open within the <=750 ms hard deadline
- automatic payload is materially below the <=3.5 KiB target
- one daemon/native explore request is used per eligible prompt
- no additional persistent process is introduced

The performance-based DO_NOT_LAND hold is cleared. Full package/native acceptance, main landing, Overdeck pin/hook rollout, and live behavioral canaries remain before the overall plan can become COMPLETE.

## Definition of done

Complete only when the front-load/explore implementation is accepted, landed to QC main, pinned/landed in Overdeck, deployed locally, and a live structural-prompt canary proves QC context is supplied automatically while non-structural prompts remain untouched.
