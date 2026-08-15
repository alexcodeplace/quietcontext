/**
 * Item 1 (honest savings ledger) + item 2 (burst feedback) — both live
 * in the same `trackResponse` choke point in src/server.ts, so they're
 * exercised together against a real spawned stdio server (same pattern
 * as tests/quietcontext-runtime.test.ts).
 */
import { describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import { loadDatabase } from "../src/db-base.js";
import { resolveSessionDbPath, SessionDB } from "../src/session/db.js";

/**
 * The ledger/burst emitters (src/session/event-emit.ts) are best-effort
 * riders on a session DB the platform's SessionStart hook already created
 * — they never create one themselves (a missing DB means "no session
 * lifecycle wired up", which they must skip silently, not fabricate).
 * Pre-seed a session_meta row the same way that hook would before
 * spawning the MCP server, so getLatestSessionId() resolves.
 */
function seedSessionDb(dataDir: string, projectDir: string): string {
  mkdirSync(join(dataDir, "sessions"), { recursive: true });
  const dbPath = resolveSessionDbPath({ projectDir, sessionsDir: join(dataDir, "sessions") });
  const db = new SessionDB({ dbPath });
  db.ensureSession(`seed-${randomUUID()}`, projectDir);
  db.close();
  return dbPath;
}

/**
 * Poll `check` until it returns a truthy value or `timeoutMs` elapses.
 * The ledger writes land via fire-and-forget setImmediate() in a separate
 * process — a fixed sleep is a race under load; poll instead.
 */
async function waitFor<T>(check: () => T | undefined | null, timeoutMs = 5000, intervalMs = 25): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    try {
      const result = check();
      if (result) return result;
    } catch {
      // SQLITE_BUSY while the server process is mid-write — retry.
    }
    if (Date.now() >= deadline) throw new Error("waitFor: timed out");
    await new Promise((r) => setTimeout(r, intervalMs));
  }
}

const ROOT = resolve(import.meta.dirname, "..");

function readResponse(child: ChildProcessWithoutNullStreams): Promise<Record<string, any>> {
  return new Promise((resolveResponse, reject) => {
    let buffer = "";
    const onData = (chunk: Buffer) => {
      buffer += chunk.toString();
      let newline = buffer.indexOf("\n");
      while (newline >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        if (!line) continue;
        try {
          const response = JSON.parse(line);
          if (response && response.id !== undefined) {
            child.stdout.off("data", onData);
            resolveResponse(response);
            return;
          }
        } catch (error) {
          child.stdout.off("data", onData);
          reject(error);
          return;
        }
      }
    };
    child.stdout.on("data", onData);
    child.once("error", reject);
  });
}

function send(child: ChildProcessWithoutNullStreams, message: Record<string, unknown>) {
  child.stdin.write(JSON.stringify(message) + "\n");
}

const BURST_HINT_SUBSTRING = "re-bills this entire conversation at cache-read prices";

describe("honest savings ledger + burst feedback (items 1+2)", () => {
  test("execute writes a tool_ledger row and a rapid 2nd call gets one burst hint, a 3rd gets none", async () => {
    const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-ledger-"));
    const projectDir = mkdtempSync(join(tmpdir(), "quietcontext-ledger-project-"));
    const dbPath = seedSessionDb(dataDir, projectDir);
    const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: {
        ...process.env,
        QUIET_CONTEXT_PLATFORM: "codex",
        QUIET_CONTEXT_DIR: dataDir,
        QUIET_CONTEXT_PROJECT_DIR: projectDir,
      },
    });

    try {
      send(child, {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "quietcontext-test", version: "1.0.0" },
        },
      });
      await readResponse(child);
      send(child, { jsonrpc: "2.0", method: "notifications/initialized", params: {} });

      const callExecute = async (id: number) => {
        send(child, {
          jsonrpc: "2.0",
          id,
          method: "tools/call",
          params: {
            name: "execute",
            arguments: { language: "javascript", code: "console.log('ledger-probe')" },
          },
        });
        const response = await readResponse(child);
        return (response.result?.content?.[0]?.text ?? "") as string;
      };

      const first = await callExecute(2);
      expect(first).not.toContain(BURST_HINT_SUBSTRING);

      const second = await callExecute(3);
      expect(second).toContain(BURST_HINT_SUBSTRING);

      const third = await callExecute(4);
      expect(third).not.toContain(BURST_HINT_SUBSTRING);

      const Database = loadDatabase();
      const rows = await waitFor(() => {
        const db = new Database(dbPath, { readonly: true });
        try {
          const found = db.prepare(
            "SELECT tool, bytes_returned, counterfactual_bytes FROM tool_ledger WHERE tool = 'execute'",
          ).all() as Array<{ tool: string; bytes_returned: number; counterfactual_bytes: number }>;
          return found.length >= 3 ? found : undefined;
        } finally {
          db.close();
        }
      });
      expect(rows.length).toBe(3);
      for (const row of rows) {
        expect(row.bytes_returned).toBeGreaterThan(0);
        // raw output here is tiny (well under the 32KB truncation baseline),
        // so counterfactual must equal the raw size, not the capped 2KB.
        expect(row.counterfactual_bytes).toBe(row.bytes_returned);
      }
    } finally {
      child.kill();
      rmSync(dataDir, { recursive: true, force: true });
      rmSync(projectDir, { recursive: true, force: true });
    }
  }, 30_000);

  test("a raw payload over the 32KB truncation baseline is capped to a 2KB counterfactual", async () => {
    const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-ledger-cap-"));
    const projectDir = mkdtempSync(join(tmpdir(), "quietcontext-ledger-cap-project-"));
    const dbPath = seedSessionDb(dataDir, projectDir);
    const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: {
        ...process.env,
        QUIET_CONTEXT_PLATFORM: "codex",
        QUIET_CONTEXT_DIR: dataDir,
        QUIET_CONTEXT_PROJECT_DIR: projectDir,
      },
    });

    try {
      send(child, {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "quietcontext-test", version: "1.0.0" },
        },
      });
      await readResponse(child);
      send(child, { jsonrpc: "2.0", method: "notifications/initialized", params: {} });

      // 40KB of stdout — well over the 32KB host truncation baseline and
      // over LARGE_OUTPUT_THRESHOLD, so the handler auto-indexes it.
      send(child, {
        jsonrpc: "2.0",
        id: 2,
        method: "tools/call",
        params: {
          name: "execute",
          arguments: {
            language: "javascript",
            code: "console.log('x'.repeat(40 * 1024))",
          },
        },
      });
      await readResponse(child);

      const Database = loadDatabase();
      const row = await waitFor(() => {
        const db = new Database(dbPath, { readonly: true });
        try {
          return db.prepare(
            "SELECT bytes_returned, counterfactual_bytes FROM tool_ledger WHERE tool = 'execute' ORDER BY id DESC LIMIT 1",
          ).get() as { bytes_returned: number; counterfactual_bytes: number } | undefined;
        } finally {
          db.close();
        }
      });
      expect(row).toBeDefined();
      expect(row.counterfactual_bytes).toBe(2 * 1024);
      expect(row.bytes_returned).toBeLessThan(row.counterfactual_bytes);
    } finally {
      child.kill();
      rmSync(dataDir, { recursive: true, force: true });
      rmSync(projectDir, { recursive: true, force: true });
    }
  }, 30_000);
});
