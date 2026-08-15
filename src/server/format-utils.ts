import type { ContentStore } from "../store.js";
import { detectPlatform } from "../adapters/detect.js";

// ─────────────────────────────────────────────────────────
// Helper: smart snippet extraction — returns windows around
// matching query terms instead of dumb truncation
//
// When `highlighted` is provided (from FTS5 `highlight()` with
// STX/ETX markers), match positions are derived from the markers.
// This is the authoritative source — FTS5 uses the exact same
// tokenizer that produced the BM25 match, so stemmed variants
// like "configuration" matching query "configure" are found
// correctly. Falls back to indexOf on raw terms when highlighted
// is absent (non-FTS codepath).
// ─────────────────────────────────────────────────────────

const STX = "\x02";
const ETX = "\x03";

/**
 * Parse FTS5 highlight markers to find match positions in the
 * original (marker-free) text. Returns character offsets into the
 * stripped content where each matched token begins.
 */
export function positionsFromHighlight(highlighted: string): number[] {
  const positions: number[] = [];
  let cleanOffset = 0;

  let i = 0;
  while (i < highlighted.length) {
    if (highlighted[i] === STX) {
      // Record position of this match in the clean text
      positions.push(cleanOffset);
      i++; // skip STX
      // Advance through matched text until ETX
      while (i < highlighted.length && highlighted[i] !== ETX) {
        cleanOffset++;
        i++;
      }
      if (i < highlighted.length) i++; // skip ETX
    } else {
      cleanOffset++;
      i++;
    }
  }

  return positions;
}

/** Strip STX/ETX markers to recover original content. */
function stripMarkers(highlighted: string): string {
  return highlighted.replaceAll(STX, "").replaceAll(ETX, "");
}

export function extractSnippet(
  content: string,
  query: string,
  maxLen = 1500,
  highlighted?: string,
): string {
  if (content.length <= maxLen) return content;

  // Derive match positions from FTS5 highlight markers when available
  const positions: number[] = [];

  if (highlighted) {
    for (const pos of positionsFromHighlight(highlighted)) {
      positions.push(pos);
    }
  }

  // Fallback: indexOf on raw query terms (non-FTS codepath)
  if (positions.length === 0) {
    const terms = query
      .toLowerCase()
      .split(/\s+/)
      .filter((t) => t.length > 2);
    const lower = content.toLowerCase();

    for (const term of terms) {
      let idx = lower.indexOf(term);
      while (idx !== -1) {
        positions.push(idx);
        idx = lower.indexOf(term, idx + 1);
      }
    }
  }

  // No matches at all — return prefix
  if (positions.length === 0) {
    return content.slice(0, maxLen) + "\n…";
  }

  // Sort positions, merge overlapping windows
  positions.sort((a, b) => a - b);
  const WINDOW = 300;
  const windows: Array<[number, number]> = [];

  for (const pos of positions) {
    const start = Math.max(0, pos - WINDOW);
    const end = Math.min(content.length, pos + WINDOW);
    if (windows.length > 0 && start <= windows[windows.length - 1][1]) {
      windows[windows.length - 1][1] = end;
    } else {
      windows.push([start, end]);
    }
  }

  // Collect windows until maxLen
  const parts: string[] = [];
  let total = 0;
  for (const [start, end] of windows) {
    if (total >= maxLen) break;
    const part = content.slice(start, Math.min(end, start + (maxLen - total)));
    parts.push(
      (start > 0 ? "…" : "") + part + (end < content.length ? "…" : ""),
    );
    total += part.length;
  }

  return parts.join("\n\n");
}

export type BatchQueryScope = "batch" | "global";

export function formatBatchQueryResults(
  store: ContentStore,
  queries: string[],
  source: string,
  maxOutput = 12 * 1024,
  scope: BatchQueryScope = "batch",
): string[] {
  const sections: string[] = [];
  let outputSize = 0;
  const emitted = new Set<string>();

  // When scope is "global", searchWithFallback receives `undefined` for the
  // source filter, which makes it query the entire persistent index instead
  // of only the chunks just produced by this batch's commands. Default
  // remains "batch" to preserve the historical behavior.
  const searchSource = scope === "global" ? undefined : source;

  for (const query of new Set(queries)) {
    if (outputSize >= maxOutput) {
      sections.push(`query: ${query}\n(output cap reached — use ctx_search(queries: ["${query}"]) for details)`);
      continue;
    }

    const results = store.searchWithFallback(query, 2, searchSource, undefined, "exact");
    sections.push(`query: ${query}`);
    outputSize += Buffer.byteLength(`query: ${query}`);
    if (results.length > 0) {
      for (const result of results) {
        const key = `${result.source}\u0000${result.title}\u0000${result.content}`;
        if (emitted.has(key)) continue;
        emitted.add(key);
        const snippet = extractSnippet(result.content, query, 600, result.highlighted);
        const section = `${result.title}\n${snippet}`;
        if (outputSize + Buffer.byteLength(section) > maxOutput) break;
        sections.push(section);
        outputSize += Buffer.byteLength(section);
      }
      continue;
    }

    sections.push("No matching sections found.");
  }

  if (scope === "global") {
    sections.push("scope: global index");
  }

  return sections;
}

// ─────────────────────────────────────────────────────────
// batch_execute runner — used by ctx_batch_execute handler
// ─────────────────────────────────────────────────────────

export interface BatchCommand { label: string; command: string; }

export interface BatchRunResult {
  outputs: string[];
  timedOut: boolean;
}

export interface BatchResponseSummary {
  commandCount: number;
  totalLines: number;
  totalBytes: number;
  indexedSections: number;
  queryCount: number;
  queryResults: string[];
  timedOut: boolean;
}

export function formatBatchResponse(summary: BatchResponseSummary): string {
  const commandWord = summary.commandCount === 1 ? "command" : "commands";
  const queryWord = summary.queryCount === 1 ? "query" : "queries";
  const timeout = summary.timedOut ? "; timed out" : "";
  const headline = `${summary.commandCount} ${commandWord}; ${summary.totalLines} lines; ${(summary.totalBytes / 1024).toFixed(1)}KB; ${summary.indexedSections} sections indexed; ${summary.queryCount} ${queryWord}${timeout}.`;
  const matches = summary.queryResults.filter((result) => result.trim().length > 0);
  return [headline, ...matches].join("\n\n");
}

export interface BatchRunOptions {
  /**
   * Total budget (concurrency=1, shared) or per-command (concurrency>1).
   * When `undefined`, no server-side timer fires — the MCP host's RPC
   * timeout governs (Issue #406).
   */
  timeout: number | undefined;
  concurrency: number;
  nodeOptsPrefix: string;
  cwd?: string;
  onFsBytes?: (bytes: number) => void;
}

export interface BatchExecutor {
  execute(input: { language: "shell"; code: string; timeout: number | undefined; cwd?: string }): Promise<{ stdout: string; timedOut?: boolean }>;
}

function quotePosixSingle(value: string): string {
  return `'${value.replace(/'/g, "'\\''")}'`;
}

function quotePowerShellSingle(value: string): string {
  return `'${value.replace(/'/g, "''")}'`;
}

export function buildBatchNodeOptionsPrefix(shellPath: string, preloadPath: string): string {
  const option = `--require ${preloadPath}`;
  const shell = shellPath.toLowerCase();
  const base = shell.split(/[\\/]/).pop() ?? shell;

  if (shell.includes("powershell") || shell.includes("pwsh")) {
    return `$env:NODE_OPTIONS=${quotePowerShellSingle(option)}; `;
  }

  if (base === "cmd" || base === "cmd.exe") {
    return `set "NODE_OPTIONS=${option.replace(/"/g, '""')}" && `;
  }

  return `NODE_OPTIONS=${quotePosixSingle(option)} `;
}

/**
 * Per-section budget for the echoed `$ <command>` line so a 50KB heredoc
 * payload cannot dominate the response body. The full command always reaches
 * the executor — only the echo is clipped (Issues #717 + #736).
 */
const COMMAND_ECHO_MAX = 500;

function truncateCommandForEcho(command: string): string {
  const cleaned = command.replace(/\s+/g, " ").trim();
  if (cleaned.length <= COMMAND_ECHO_MAX) return cleaned;
  return cleaned.slice(0, COMMAND_ECHO_MAX) + "…";
}

/**
 * Default execution timeout (ms) applied ONLY under Antigravity CLI (`agy`).
 * agy does not enforce an MCP RPC timeout, so a ctx_execute with a runaway or
 * blocking script hangs forever — the host never kills it and the user must
 * interrupt. Every other host enforces its own RPC timeout, so we keep the
 * no-server-timer behavior there (Issue #406 — long builds need an unbounded
 * run). A caller can still pass an explicit `timeout` to override on any host.
 */
export const AGY_DEFAULT_EXEC_TIMEOUT_MS = 120_000;
export function resolveExecTimeout(timeout: number | undefined): number | undefined {
  if (timeout !== undefined) return timeout;
  // Only agy gets a default — every other host enforces its own RPC timeout, so
  // keep the unbounded behavior there. Detected via the env the agy bundle pins
  // (QUIET_CONTEXT_PLATFORM=antigravity-cli). Tunable via QUIET_CONTEXT_AGY_EXEC_TIMEOUT_MS.
  if (detectPlatform().platform !== "antigravity-cli") return undefined;
  const override = Number(process.env.QUIET_CONTEXT_AGY_EXEC_TIMEOUT_MS);
  return Number.isFinite(override) && override > 0 ? override : AGY_DEFAULT_EXEC_TIMEOUT_MS;
}

export function buildExecuteEcho(_language: string, _code: string, _path?: string): string {
  return "";
}

export function formatCommandOutput(label: string, command: string, raw: string, onFsBytes?: (bytes: number) => void): string {
  let output = raw || "(no output)";
  const fsMatches = output.matchAll(/__CM_FS__:(\d+)/g);
  let cmdFsBytes = 0;
  for (const m of fsMatches) cmdFsBytes += parseInt(m[1]);
  if (cmdFsBytes > 0) {
    onFsBytes?.(cmdFsBytes);
    output = output.replace(/__CM_FS__:\d+\n?/g, "");
  }
  // Echo the executed command below the section heading so per-chunk
  // indexed content retains provenance for later ctx_search hits
  // (Issues #717 + #736).
  const echoed = truncateCommandForEcho(command);
  return `# ${label}\n\n$ ${echoed}\n\n${output}\n`;
}

export function combineExecOutput(result: { stdout?: string; stderr?: string }): string {
  const stdout = result.stdout || "";
  const stderr = result.stderr || "";
  if (!stderr) return stdout;
  if (!stdout) return stderr;
  return `${stdout}${stdout.endsWith("\n") ? "" : "\n"}${stderr}`;
}

export function capUtf8(value: string, maxBytes: number): string {
  if (maxBytes <= 0) return "";
  const bytes = Buffer.from(value);
  if (bytes.length <= maxBytes) return value;
  if (maxBytes < Buffer.byteLength("…")) return "";
  let end = Math.max(0, maxBytes - Buffer.byteLength("…"));
  while (end > 0 && (bytes[end] & 0xc0) === 0x80) end--;
  return bytes.subarray(0, end).toString("utf8") + "…";
}
