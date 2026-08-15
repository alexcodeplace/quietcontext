import { spawnSync, type SpawnSyncOptions, type SpawnSyncReturns } from "node:child_process";
import { z } from "zod";
import { server, trackResponse } from "../server.js";

// ── ctx_insight process helpers ──────────────────────────────────────────────
// Cross-platform process helpers used by ctx_insight (below) and the dashboard
// launcher in cli.ts. All entry points use argv arrays — never `sh -c <string>`
// — so caller-derived values cannot escape into shell context. See issue #441.
//
// `browserOpenArgv` is duplicated as a private 16-LOC copy in cli.ts to avoid
// pulling server.ts top-level boot side effects into the cli bundle.

export type SpawnSyncFn = (
  cmd: string,
  args: readonly string[],
  opts?: SpawnSyncOptions,
) => SpawnSyncReturns<string | Buffer>;

export type BrowserOpenResult =
  | { ok: true; method: string }
  | { ok: false; method: "none"; reason: string };

export type KillResult = {
  killedPids: string[];
  attemptedPids: string[];
  errors: string[];
};

// Hard upper bound on every helper-internal spawnSync call. Caps tail-latency
// when an external binary hangs (xdg-open waiting for an X11 session, lsof
// stalling on /proc, taskkill blocking on an unresponsive process, etc.) so
// the MCP tool surfaces a diagnostic instead of blocking the agent loop.
// 5s is comfortably above the 99th-percentile completion of every command we
// invoke; anything past that is hung.
const HELPER_SPAWN_TIMEOUT_MS = 5000;

// Returns the argv attempts for opening `url` on `platform`, in fall-back order.
// Pure data — no I/O.
export function browserOpenArgv(
  url: string,
  platform: NodeJS.Platform,
): readonly { cmd: string; args: readonly string[] }[] {
  if (platform === "darwin") return [{ cmd: "open", args: [url] }];
  if (platform === "win32") {
    // `start` is a cmd.exe builtin; the empty title arg ("") prevents the URL
    // from being consumed as the window title.
    return [{ cmd: "cmd", args: ["/c", "start", "", url] }];
  }
  // linux/bsd: try xdg-open, then sensible-browser (Debian/Ubuntu).
  return [
    { cmd: "xdg-open", args: [url] },
    { cmd: "sensible-browser", args: [url] },
  ];
}

// Opens a browser synchronously, waiting for each attempt to complete.
// Returns a structured result so callers can surface auto-open failures
// to the user instead of falsely reporting success.
export function openBrowserSync(
  url: string,
  platform: NodeJS.Platform = process.platform,
  runner: SpawnSyncFn = spawnSync,
): BrowserOpenResult {
  const attempts = browserOpenArgv(url, platform);
  const errors: string[] = [];
  for (const { cmd, args } of attempts) {
    try {
      const r = runner(cmd, args, { stdio: "ignore", timeout: HELPER_SPAWN_TIMEOUT_MS });
      // Treat signal-kill (status === null) and any non-zero status as failure
      // so the next fallback fires.
      if (!r.error && r.status === 0) return { ok: true, method: cmd };
      const reason = r.error?.message ?? `status=${r.status === null ? "signaled" : r.status}`;
      errors.push(`${cmd}: ${reason}`);
    } catch (e) {
      errors.push(`${cmd}: ${e instanceof Error ? e.message : String(e)}`);
    }
  }
  return { ok: false, method: "none", reason: errors.join("; ") };
}

// Kills any process listening on `port`. Returns a structured result so
// the caller can distinguish between (a) port was free, (b) kill succeeded,
// (c) kill failed (perms, missing binary, or per-pid failure mid-loop).
//
// On Windows the netstat parser is locale-independent: the STATE column
// ("LISTENING" / "ESTABLISHED" / ...) is translated on non-English Windows
// (Windows-FR shows "À l'écoute", Windows-DE "ABHÖREN", etc.), but the REMOTE
// ADDRESS column is not. A listening TCP socket always has remote
// "0.0.0.0:0" (IPv4) or "[::]:0" (IPv6); a connected one has a real
// addr:port. We therefore key off the remote column instead of the state
// string. This also rules out the pre-fix bug where matching only the local
// port number cross-matched a remote :port from an outbound connection and
// taskkill'd an unrelated process.
export function killProcessOnPort(
  port: number,
  platform: NodeJS.Platform = process.platform,
  runner: SpawnSyncFn = spawnSync,
): KillResult {
  const result: KillResult = { killedPids: [], attemptedPids: [], errors: [] };
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    result.errors.push(`invalid port: ${port}`);
    return result;
  }

  try {
    if (platform === "win32") {
      const r = runner("netstat", ["-ano"], {
        encoding: "utf-8",
        stdio: ["ignore", "pipe", "ignore"],
        timeout: HELPER_SPAWN_TIMEOUT_MS,
      });
      if (r.error) {
        result.errors.push(`netstat: ${r.error.message}`);
        return result;
      }
      if (r.status !== 0 || typeof r.stdout !== "string") return result;

      const portSuffix = `:${port}`;
      const pids = new Set<string>();
      for (const rawLine of r.stdout.split(/\r?\n/)) {
        const line = rawLine.trim();
        if (!line) continue;
        const tokens = line.split(/\s+/);
        // netstat -ano LISTENING row (en-US): "TCP  0.0.0.0:4747  0.0.0.0:0  LISTENING  1234"
        // The STATE column is locale-translated and may itself contain spaces
        // (Windows-FR `À l'écoute` splits into two tokens), so we cannot index
        // STATE by position. PID is always the trailing column; PROTO/LOCAL/
        // REMOTE are the first three. We anchor on those + a remote-wildcard
        // check that's locale-independent.
        if (tokens.length < 5) continue;
        const proto = tokens[0];
        const local = tokens[1];
        const remote = tokens[2];
        const pid = tokens[tokens.length - 1];
        if (proto !== "TCP") continue;
        if (!local.endsWith(portSuffix)) continue;
        // Listening sockets carry a wildcard remote; anything else is a
        // connection (and matching it would kill an unrelated process).
        if (remote !== "0.0.0.0:0" && remote !== "[::]:0") continue;
        if (!/^\d+$/.test(pid)) continue;
        pids.add(pid);
      }
      for (const pid of pids) {
        result.attemptedPids.push(pid);
        try {
          const k = runner("taskkill", ["/F", "/PID", pid], {
            stdio: "ignore",
            timeout: HELPER_SPAWN_TIMEOUT_MS,
          });
          if (k.error || k.status !== 0) {
            result.errors.push(
              `taskkill ${pid}: ${k.error?.message ?? `status=${k.status}`}`,
            );
          } else {
            result.killedPids.push(pid);
          }
        } catch (e) {
          result.errors.push(`taskkill ${pid}: ${e instanceof Error ? e.message : String(e)}`);
        }
      }
    } else {
      const r = runner("lsof", ["-ti", `:${port}`], {
        encoding: "utf-8",
        stdio: ["ignore", "pipe", "ignore"],
        timeout: HELPER_SPAWN_TIMEOUT_MS,
      });
      if (r.error) {
        // ENOENT (lsof not installed) is a real diagnostic; surface it.
        result.errors.push(`lsof: ${r.error.message}`);
        return result;
      }
      // lsof exits 1 with empty stdout when the port is free — not an error.
      if (r.status !== 0 || typeof r.stdout !== "string") return result;

      const pids = r.stdout.split(/\r?\n/).filter(p => /^\d+$/.test(p));
      for (const pid of pids) {
        result.attemptedPids.push(pid);
        try {
          const k = runner("kill", [pid], {
            stdio: "ignore",
            timeout: HELPER_SPAWN_TIMEOUT_MS,
          });
          if (k.error || k.status !== 0) {
            result.errors.push(
              `kill ${pid}: ${k.error?.message ?? `status=${k.status}`}`,
            );
          } else {
            result.killedPids.push(pid);
          }
        } catch (e) {
          result.errors.push(`kill ${pid}: ${e instanceof Error ? e.message : String(e)}`);
        }
      }
    }
  } catch (e) {
    result.errors.push(e instanceof Error ? e.message : String(e));
  }
  return result;
}

// ── ctx-insight: open the hosted Insight dashboard ───────────────────────────
// Insight pivoted from a locally-built dashboard to the hosted B2B product at
// context-mode.com/insight (the landing page is the single source of truth).
// The tool now simply opens that URL in the user default browser via the same
// cross-platform helper (openBrowserSync) used elsewhere.
//
// Dead code: never called (see docs/legacy-tools-dormant-boundary.md). Moved
// verbatim from src/server.ts as part of the core-extraction carve lane.
export function registerLegacyInsightTool(): void {
const INSIGHT_URL = "https://context-mode.com/insight";

server.registerTool(
  "ctx_insight",
  {
    title: "Open Insight Dashboard",
    // #846: opens a hosted dashboard URL in the browser — an external side
    // effect (open world), not a read-only query; safe to repeat.
    annotations: {
      readOnlyHint: false,
      destructiveHint: false,
      idempotentHint: true,
      openWorldHint: true,
    },
    description:
      "Opens the context-mode Insight dashboard (https://context-mode.com/insight) in your " +
      "default browser — a dashboard launcher for the hosted analytics layer, not a Q&A engine. " +
      "Insight surfaces per-engineer productive rate, retry waste, blocker detection, and " +
      "role-narrowed views for CTO, EM, IC, CISO, FinOps, and DevOps. " +
      "For natural-language queries over your indexed content, use ctx_search.",
    inputSchema: z.object({}),
  },
  async () => {
    const open = openBrowserSync(INSIGHT_URL);
    const text = open.ok
      ? `Opening Insight in your browser: ${INSIGHT_URL}`
      : `Could not auto-open your browser (${open.reason}).\nOpen Insight manually: ${INSIGHT_URL}`;
    return trackResponse("ctx_insight", {
      content: [{ type: "text" as const, text }],
    });
  },
);

}
