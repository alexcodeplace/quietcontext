# QuietContext

QuietContext is a Codex-focused MCP server for keeping raw data outside model context.

No prompt injection. No session-memory narration. No analytics tools. Six tools only.

## Status

Local fork under active development. No canonical public repository or package release is configured.

Upstream source: [mksglu/context-mode](https://github.com/mksglu/context-mode), licensed under Elastic License 2.0.

## Tools

| Tool | Contract |
|---|---|
| `batch` | Run related shell commands, index raw output, return bounded query matches. |
| `execute` | Run code in sandbox; reuse long programs through short script references. |
| `exec-file` | Process workspace files; reuse one cached program across paths. |
| `index` | Index content, files, or bounded directories into FTS5. |
| `search` | Search indexed content or retrieve exact result references. |
| `fetch-index` | Fetch and index URLs without returning raw pages. |

Public names above are canonical. Do not use inherited `ctx_*` names.

## Codex setup

Install package from this checkout, then add:

```toml
[mcp_servers.quietcontext]
command = "quietcontext"
```

Recommended project instruction:

```markdown
# quietcontext

Use `execute` or `exec-file` for large data, `batch` for related commands, and `search` for indexed content. Keep raw output in sandbox; return only derived results.
```

Hooks are intentionally empty. Codex owns compaction and session continuity.

## Token budgets

- MCP `tools/list`: ≤4 KiB serialized.
- `execute` and `exec-file`: never echo submitted code; programs ≥256 bytes return reusable `[s:<ref>]` handles.
- Direct execution output: ≤8 KiB; larger output is indexed and returned as a pointer.
- `search`: titles + `[r:<id>]` references by default; `preview: true` adds ≤600 characters per unique result.
- `batch`: ≤12 KiB total.
- `fetch-index`: no content preview by default.
- `execute`, `exec-file`, `search`, and `batch` accept `max_bytes` below hard caps.
- Script references are process-local; search references support exact follow-up retrieval.

## Development

```bash
npm install
npm run typecheck
npm test
npm run benchmark:quiet
```

`npm test` is the authoritative QuietContext contract suite. Inherited Context Mode platform, hook, analytics, and session-continuity tests are not QuietContext product contracts.

## Architecture

- `src/server.ts`: explicit six-tool MCP server.
- `src/executor.ts`: sandboxed polyglot execution.
- `src/store.ts`: FTS5 indexing, ranking, exact reference retrieval.
- `src/security.ts`: workspace and command policy.
- `tests/quietcontext-*.test.ts`: public surface, runtime, and token-budget contracts.

Design: [docs/specs/2026-07-13-quietcontext-token-budget-design.md](docs/specs/2026-07-13-quietcontext-token-budget-design.md)

## License

Elastic License 2.0. Preserve upstream copyright and license notices.
