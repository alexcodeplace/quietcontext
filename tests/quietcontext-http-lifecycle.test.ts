/**
 * HTTP daemon store-isolation and shutdown invariants beyond what
 * quietcontext-http.test.ts already pins (per-root isolation, auth,
 * root validation). The shared daemon owns one durable per-project ContentStore
 * while stdio fallback children remain ephemeral. Daemon shutdown must close
 * every root cleanly without deleting durable project indexes, and idle-store
 * eviction must close/reopen a handle without data loss.
 */
import { afterAll, describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");
const cleanupTargets: string[] = [];

afterAll(() => {
  for (const dir of cleanupTargets.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function startDaemon(env: Record<string, string>): Promise<{
  daemon: ChildProcessWithoutNullStreams;
  port: number;
}> {
  return new Promise((resolvePort, reject) => {
    const daemon = spawn(process.execPath, [join(ROOT, "start-http.mjs")], {
      cwd: ROOT,
      env: { ...process.env, ...env },
      stdio: ["ignore", "pipe", "pipe"],
    }) as ChildProcessWithoutNullStreams;
    let stderr = "";
    const onData = (chunk: Buffer) => {
      stderr += chunk.toString();
      const m = /listening on http:\/\/127\.0\.0\.1:(\d+)\/mcp/.exec(stderr);
      if (m) {
        daemon.stderr.off("data", onData);
        resolvePort({ daemon, port: Number(m[1]) });
      }
    };
    daemon.stderr.on("data", onData);
    daemon.once("exit", (code) => reject(new Error(`daemon exited early (${code}): ${stderr}`)));
    daemon.once("error", reject);
  });
}

let nextId = 1;
async function callTool(
  port: number,
  token: string,
  root: string,
  name: string,
  args: Record<string, unknown>,
): Promise<void> {
  const res = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      authorization: `Bearer ${token}`,
      "x-quietcontext-root": root,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: nextId++,
      method: "tools/call",
      params: { name, arguments: args },
    }),
  });
  const text = await res.text();
  const dataLine = text.split("\n").find((l) => l.startsWith("data: "));
  const body = JSON.parse(dataLine ? dataLine.slice(6) : text);
  expect(res.status, JSON.stringify(body)).toBe(200);
  expect(body?.result?.isError, JSON.stringify(body)).not.toBe(true);
}

function waitForExit(
  daemon: ChildProcessWithoutNullStreams,
  timeoutMs: number,
): Promise<{ code: number | null; signal: NodeJS.Signals | null } | "TIMEOUT"> {
  return Promise.race([
    new Promise<{ code: number | null; signal: NodeJS.Signals | null }>((resolveExit) =>
      daemon.once("exit", (code, signal) => resolveExit({ code, signal })),
    ),
    new Promise<"TIMEOUT">((resolveTimeout) => setTimeout(() => resolveTimeout("TIMEOUT"), timeoutMs)),
  ]);
}

describe("quietcontext HTTP daemon store lifecycle", () => {
  test(
    "shutdown closes every active durable root without deleting project indexes",
    async () => {
      const scratch = mkdtempSync(join(tmpdir(), "qc-http-lifecycle-"));
      const runtimeDir = mkdtempSync(join(tmpdir(), "qc-http-lifecycle-runtime-"));
      cleanupTargets.push(scratch, runtimeDir);

      const rootA = join(scratch, "project-a");
      const rootB = join(scratch, "project-b");
      const rootC = join(scratch, "project-c");
      for (const dir of [rootA, rootB, rootC]) mkdirSync(dir, { recursive: true });
      const tokenFile = join(scratch, "daemon.token");

      const { daemon, port } = await startDaemon({
        QUIET_CONTEXT_DAEMON_PORT: "0",
        QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
        QUIET_CONTEXT_DIR: join(scratch, "data"),
        TMPDIR: runtimeDir,
        TEMP: runtimeDir,
        TMP: runtimeDir,
      });
      const token = readFileSync(tokenFile, "utf8").trim();

      await Promise.all([
        callTool(port, token, rootA, "index", { source: "a", content: "alpha content" }),
        callTool(port, token, rootB, "index", { source: "b", content: "beta content" }),
        callTool(port, token, rootC, "index", { source: "c", content: "gamma content" }),
      ]);

      const contentDir = join(scratch, "data", "content");
      const durableDbFiles = () => readdirSync(contentDir).filter((f) => f.endsWith(".db"));
      expect(durableDbFiles()).toHaveLength(3);
      // Daemon content stores no longer live under TMPDIR.
      expect(readdirSync(runtimeDir).filter((f) => f.endsWith(".db"))).toEqual([]);

      daemon.kill("SIGTERM");
      const exitResult = await waitForExit(daemon, 10_000);
      expect(exitResult).not.toBe("TIMEOUT");
      expect((exitResult as { code: number | null }).code).toBe(0);
      expect(durableDbFiles()).toHaveLength(3);
    },
    30_000,
  );

  test(
    "idle-store eviction closes without unlinking, and a later touch reopens the same store transparently",
    async () => {
      const scratch = mkdtempSync(join(tmpdir(), "qc-http-evict-"));
      const runtimeDir = mkdtempSync(join(tmpdir(), "qc-http-evict-runtime-"));
      cleanupTargets.push(scratch, runtimeDir);

      const rootA = join(scratch, "project-a");
      mkdirSync(rootA, { recursive: true });
      const tokenFile = join(scratch, "daemon.token");

      const { daemon, port } = await startDaemon({
        QUIET_CONTEXT_DAEMON_PORT: "0",
        QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
        QUIET_CONTEXT_DIR: join(scratch, "data"),
        TMPDIR: runtimeDir,
        TEMP: runtimeDir,
        TMP: runtimeDir,
        // Fast sweep so the test doesn't wait out real 30-minute windows.
        QUIET_CONTEXT_STORE_IDLE_EVICT_MS: "150",
        QUIET_CONTEXT_STORE_EVICT_SWEEP_MS: "50",
      });
      const token = readFileSync(tokenFile, "utf8").trim();

      try {
        await callTool(port, token, rootA, "index", {
          source: "evict-probe",
          content: "the eviction survivor marker is quetzal-hologram-7734",
        });

        const contentDir = join(scratch, "data", "content");
        const durableDbFiles = () => readdirSync(contentDir).filter((f) => f.endsWith(".db"));
        expect(durableDbFiles()).toHaveLength(1);
        expect(readdirSync(runtimeDir).filter((f) => f.endsWith(".db"))).toEqual([]);

        // Sit idle for several sweep cycles beyond the idle threshold —
        // the store MUST be closed (not unlinked) by the time this returns.
        await new Promise((r) => setTimeout(r, 500));
        expect(durableDbFiles()).toHaveLength(1); // close-only: durable file survives eviction

        // Touch again: getStore() must reopen the SAME file and find the
        // content indexed before eviction, not start from an empty store.
        const res = await fetch(`http://127.0.0.1:${port}/mcp`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            accept: "application/json, text/event-stream",
            authorization: `Bearer ${token}`,
            "x-quietcontext-root": rootA,
          },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: nextId++,
            method: "tools/call",
            params: { name: "search", arguments: { queries: ["quetzal-hologram"], preview: true } },
          }),
        });
        const text = await res.text();
        const dataLine = text.split("\n").find((l) => l.startsWith("data: "));
        const body = JSON.parse(dataLine ? dataLine.slice(6) : text);
        const resultText = (body?.result?.content ?? [])
          .map((c: { text?: string }) => c.text ?? "")
          .join("\n");
        expect(resultText).toContain("quetzal-hologram-7734");
      } finally {
        daemon.kill("SIGTERM");
        await waitForExit(daemon, 10_000);
      }
    },
    30_000,
  );
});
