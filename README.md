# QuietContext

QuietContext is a quiet, token-saving fork of [mksglu/context-mode](https://github.com/mksglu/context-mode): an MCP server for keeping raw data outside model context. The seven-tool surface is a deliberately leaner, more token-efficient way of doing what context-mode does — stripping the prompt injection, session-memory narration, analytics, and other context waste that had accumulated in a plugin whose whole purpose is saving tokens.

No prompt injection. No session-memory narration. No analytics tools. Seven tools only, pinned by contract tests.

Served over the stateless operation mode of the MCP Streamable HTTP transport (August 2026 spec revision): one shared local daemon replaces per-session server processes.

## Why the shared daemon

Measured on a workstation running dozens of concurrent agent sessions (all numbers from real /proc sampling, 2026-08-15):

- Before: 36 resident processes (17 node + 17 bun pairs), 2,214 MiB total RSS — ~130 MiB per session, spawned per session and resident for the session's lifetime. Extrapolated from that measured per-session cost: ~6.5 GiB at 50 sessions.
- After: 1 process, ~80 MiB, flat regardless of session count. New sessions spawn zero QuietContext processes.
- Session-start cost: was a node+bun pair per session; now zero processes spawned.
- Idle cost: clients hold zero daemon resources while idle — the transport is stateless, sockets close within ~9 s, and no protocol sessions are held.

Claude Code v2.1.221+ caches discovery for HTTP servers and connects on first tool use; the daemon supports that cached-discovery / connect-on-first-use behavior for remote servers.

## Starting the daemon

Linux: `systemd/quietcontext-daemon.service` (user unit).

Windows: `windows/Install-QuietContextDaemon.ps1` registers an equivalent
per-user logon task — see [windows/README.md](windows/README.md).

## Concurrency and isolation

Multiple sessions in different working directories are served concurrently by one daemon, with per-working-root store isolation — verified with two simultaneous real sessions receiving correct per-root answers through a single daemon PID.

## Security posture

Loopback-only bind (127.0.0.1). Bearer token read from a 0600 file, checked in constant time. Requests without a valid absolute working root are rejected. Local MCP daemons that skip auth are a known bad pattern; this one does not.

## Reversibility

The stdio path still ships and works. Rollback is two steps: stop the daemon, restore the stdio manifest entry. Nothing else changes.

## Tools

Small on purpose: seven tools, with the surface and byte budget pinned by contract tests.

| Tool | Contract |
|---|---|
| `batch` | Run related shell commands, index raw output, return bounded query matches. |
| `repo` | Map a repository or find symbols, references, and file outlines without dumping source files into context. |
| `execute` | Run code in sandbox; reuse long programs through short script references. |
| `exec-file` | Process workspace files; reuse one cached program across paths. |
| `index` | Index content, files, or bounded directories into FTS5. |
| `search` | Search indexed content or retrieve exact result references. |
| `fetch-index` | Fetch and index URLs without returning raw pages. |

Public names above are canonical. Do not use inherited `ctx_*` names.


## Local `qc` command

The package also ships a small local front door for Bash/tool-hook use:

```sh
qc run -- rg -n "needle" src
qc repo map
qc repo symbol MyType
printf '%s\n' "large reusable context" | qc index --stdin --source notes/demo --project "$PWD"
qc search reusable --project "$PWD" --full
qc status
qc doctor
```

`context-mode` remains an executable compatibility alias for pre-existing platform hook configurations. New hook configurations use `qc hook <platform> <event>`; new user-facing local workflows use `qc`.

`qc run -- ...` executes argv directly through the same native filtering engine used by supported MCP shell commands. It preserves the child exit code. When filtering omits raw text, QuietContext retains bounded exact evidence and indexes searchable text so `search` can recover an omitted line. `qc repo` exposes the same native repository map/symbol/reference/outline engine as the MCP `repo` tool. `qc index` and `qc search` are local front doors to QuietContext's project-scoped FTS5 store; `qc index --stdin` accepts at most the normal per-source indexing cap and rejects invalid UTF-8 instead of silently indexing binary data.

The existing Claude/Codex `PreToolUse` hook contains an opt-in `qc` routing branch and **does not enable it automatically**. Run `qc routing enable` to activate it (or `qc routing disable` to remove the marker). `QUIET_CONTEXT_QC_BASH_ROUTING=0` is an emergency bypass. When active, simple noisy commands are routed through `qc`; uncertain shell syntax passes through unchanged. Modern Codex can rewrite transparently, while current Claude Code uses an enforceable deny with an exact `qc run -- ...` retry because its Bash hook does not honor command substitution.

When the Codex adapter installs its user-level hook, the canonical entry is `qc hook codex pretooluse` with the exact supported matcher:

```json
{
  "PreToolUse": [{
    "matcher": "local_shell|shell|shell_command|exec_command|Bash|Shell|apply_patch|Edit|Write|grep_files|ctx_execute|ctx_execute_file|ctx_batch_execute|ctx_fetch_and_index|ctx_search|ctx_index|mcp__",
    "hooks": [{ "type": "command", "command": "qc hook codex pretooluse" }]
  }]
}
```

The packaged Codex plugin keeps its automatic hook manifest empty; installation into a user's `hooks.json` remains an explicit adapter/setup action. Legacy `context-mode hook ...` entries are recognized and replaced rather than duplicated.

## Token budgets

- MCP `tools/list`: ≤4 KiB serialized.
- `execute` and `exec-file`: never echo submitted code; programs ≥256 bytes return reusable `[s:<ref>]` handles.
- Direct execution output: ≤8 KiB; larger output is indexed and returned as a pointer.
- `search`: titles + `[r:<id>]` references by default; `preview: true` adds ≤600 characters per unique result.
- `batch`: ≤12 KiB total.
- `fetch-index`: no content preview by default.
- `execute`, `exec-file`, `search`, and `batch` accept `max_bytes` below hard caps.
- Script references are process-local; search references support exact follow-up retrieval.

## Known limitations

- Per-session usage stats aggregate daemon-wide (the daemon has no per-session identity).
- A daemon restart interrupts in-flight background executions across all sessions; the outage is bounded by systemd `Restart=on-failure`.
- A handful of maintenance tools (stats, doctor, upgrade, purge, insight) remain stdio-only for now.
- Interactive-session lazy connect was not directly measured (the measurement box was serving live sessions continuously); the idle-cost numbers above are the verified equivalent.
