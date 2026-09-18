# QuietContext

QuietContext keeps large command outputs, files and reference material outside an agent's conversation, then returns compact results or searchable references. Its local `qc` command filters shell output and maps repositories; its MCP server exposes seven tools for execution, repository navigation, indexing and retrieval.

Use it when a failing test run fills the chat with thousands of lines, an agent repeatedly rereads the same documentation, or several worktrees need separate searchable indexes. It reduces what is returned to the model; it does not make an inaccurate answer correct or isolate arbitrary commands from your operating-system permissions.

This is a fork of [mksglu/context-mode](https://github.com/mksglu/context-mode). The maintained surface avoids additional session-memory narration and analytics tools. That design choice is not a guarantee against prompt injection in the content an agent reads.

## Install the packaged release

The documented package is **1.1.0-rc.3**, a release candidate, not a stable 1.1.0 release. Use Node.js **22.5 or newer** and npm. The native `qc` release targets are **Linux x64 and Windows x64**; do not assume the same archive supports macOS or ARM. SQLite's native dependency may need a compiler toolchain if a matching prebuilt addon is unavailable.

Open the [release assets](https://github.com/alexcodeplace/quietcontext/releases/tag/v1.1.0-rc.3), download `quietcontext-1.1.0-rc.3.tgz` and `SHA256SUMS`, and compare the archive's SHA-256 with the published value before installing. For example, from the download directory on Linux:

```sh
sha256sum -c SHA256SUMS
npm install -g ./quietcontext-1.1.0-rc.3.tgz
qc --version
qc status
```

On Windows, compare `Get-FileHash .\quietcontext-1.1.0-rc.3.tgz -Algorithm SHA256` with `SHA256SUMS`, then run the same npm and `qc` commands. The archive contains the staged native runtime that a plain source clone does not. A source build and a downloaded release package are not interchangeable installation shortcuts.

A successful `qc status` prints the package, native-runtime and protocol versions. It verifies the packaged runtime; it does not establish that an MCP client has connected or a shared daemon is running. `qc doctor` provides a wider diagnostic report whose client/hook findings depend on your setup.

## First use: local CLI

From the project you want to inspect:

```sh
qc repo map
qc run -- git status --short
qc index README.md --source project-readme
qc search installation --source project-readme --full
```

Replace `installation` with a word in your README. An empty search is a real no-match result, not an installation error. For a noisy test command, use `qc run -- npm test` (or your actual project test command): the child exit code is preserved, and omitted output remains available through bounded retained evidence/search. `qc repo symbol MyType` and `qc repo references MyType` help locate a symbol without dumping every source file.

## Connect an MCP client

For a simple stdio connection, configure your MCP client to launch **`quietcontext`**, not `qc`; `qc` is the human-facing CLI. The command must resolve on the PATH seen by that client. Confirm its installed location first and use an absolute executable/Node entrypoint when required by the client.

For example, a Codex stdio entry is:

```toml
[mcp_servers.quietcontext]
command = "quietcontext"
```

Start a new client session and confirm discovery of the seven tools below. Ask it to index one non-sensitive document and search for a phrase you know is present before granting a broader workflow. Do not add a second server or rewrite existing client configuration blindly.

The optional shared HTTP daemon replaces per-session server processes. Its Linux user-unit template contains an installation-specific `WorkingDirectory` and `ExecStart`; adapt both to the actual installed package before enabling it. Windows has a separate per-user task installer described below. Keep authentication, an explicit working root and a loopback-only listener; the daemon is not a public service.

## Ask an agent to install and set it up

```text
Set up QuietContext for this project using
https://github.com/alexcodeplace/quietcontext and its current README.
Inspect my OS/architecture, Node/npm and existing MCP or qc installation first.
Use the documented release archive and verify its published SHA-256; do not
substitute the unrelated upstream context-mode package or assume macOS/ARM
has a native qc release. Tell me that the current documented release is an RC.
Check qc --version and qc status, map a disposable repository, and prove that
indexing a small text file and searching a known phrase returns that content.
Configure one stdio quietcontext entry in my selected agent, preserving its
other settings. Ask before choosing a client, enabling a shared daemon,
turning on Bash routing, or restarting sessions. Keep daemon auth and loopback
binding; do not print tokens or index secrets. Finish with usage examples,
actual verification results, changed paths, and rollback instructions.
```

## Why the shared daemon

Measured on a workstation running dozens of concurrent agent sessions (all numbers from real /proc sampling, 2026-08-15):

- Before: 36 resident processes (17 node + 17 bun pairs), 2,214 MiB total RSS — ~130 MiB per session, spawned per session and resident for the session's lifetime. Extrapolated from that measured per-session cost: ~6.5 GiB at 50 sessions.
- After: 1 process, ~80 MiB in that sample. New sessions did not spawn another QuietContext process; memory use can still grow with active work and indexed data.
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
qc callers MyType.method
qc callees MyType.method --depth 2
qc impact MyType.method --depth 3
qc deps src/service.ts
qc path requestHandler saveRecord
printf '%s\n' "large reusable context" | qc index --stdin --source notes/demo --project "$PWD"
qc search reusable --project "$PWD" --full
qc status
qc doctor
```

`context-mode` remains an executable compatibility alias for pre-existing platform hook configurations. `ft` is also shipped as a temporary executable alias to `qc` for Fewtok cutovers; its old repository-navigation spellings (`ft map`, `ft sym`, `ft refs`, `ft outline`) are accepted directly. New hook configurations use `qc hook <platform> <event>`; new user-facing local workflows use `qc`.

`qc run -- ...` executes argv directly through the same native filtering engine used by supported MCP shell commands. It preserves the child exit code. When filtering omits raw text, QuietContext retains bounded exact evidence and indexes searchable text so `search` can recover an omitted line. `qc repo` exposes the same native repository engine as the MCP `repo` tool. `qc map` stays a compact orientation view; JavaScript/TypeScript, Python, Rust, and Go are also indexed into a bounded semantic graph for resolved references, callers, callees, impact, dependencies, dependents, and shortest dependency paths. Ambiguous same-named definitions are kept separate rather than guessed, and `--file` can narrow a CLI symbol query. The compact MCP `repo` surface uses `symbol@path` to disambiguate colliding definitions and `from -> to` as the `path` target. `qc index` and `qc search` are local front doors to QuietContext's project-scoped FTS5 store; `qc index --stdin` accepts at most the normal per-source indexing cap and rejects invalid UTF-8 instead of silently indexing binary data.

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
- Maintenance commands and inherited compatibility code are not additional advertised MCP tools. Use `qc status`/`qc doctor` for local checks; the public discovery contract remains the seven-tool list.
- Interactive-session lazy connect was not directly measured (the measurement box was serving live sessions continuously); the idle-cost numbers above are the verified equivalent.

## License

Elastic License 2.0, not MIT. Preserve the upstream notices and review [LICENSE](LICENSE) before redistributing or offering a hosted service. The bundled native engine has additional provenance and license notices under `native/qc/`.
