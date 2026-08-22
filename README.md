# QuietContext

QuietContext is a quiet, token-saving fork of [mksglu/context-mode](https://github.com/mksglu/context-mode): an MCP server for keeping raw data outside model context. The six-tool surface is a deliberately leaner, more token-efficient way of doing what context-mode does — stripping the prompt injection, session-memory narration, analytics, and other context waste that had accumulated in a plugin whose whole purpose is saving tokens.

No prompt injection. No session-memory narration. No analytics tools. Six tools only, pinned by contract tests.

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

Small on purpose: six tools, and the surface is pinned by contract tests (29 contract+HTTP tests, 131 store tests).

| Tool | Contract |
|---|---|
| `batch` | Run related shell commands, index raw output, return bounded query matches. |
| `execute` | Run code in sandbox; reuse long programs through short script references. |
| `exec-file` | Process workspace files; reuse one cached program across paths. |
| `index` | Index content, files, or bounded directories into FTS5. |
| `search` | Search indexed content or retrieve exact result references. |
| `fetch-index` | Fetch and index URLs without returning raw pages. |

Public names above are canonical. Do not use inherited `ctx_*` names.

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
