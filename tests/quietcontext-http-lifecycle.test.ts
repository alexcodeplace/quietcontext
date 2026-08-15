/**
 * HTTP daemon store-isolation and shutdown invariants beyond what
 * quietcontext-http.test.ts already pins (per-root isolation, auth,
 * root validation). getStore() in src/server.ts creates one ephemeral,
 * process-scoped ContentStore per root (ContentStore.cleanup() deletes
 * its own db/-wal/-shm files); the daemon calls releaseProcessResources()
 * on SIGTERM/SIGINT (start-http.mjs), which must close and delete every
 * root's store with zero leaks even when several roots are active at
 * once in the same daemon process.
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
    "shutdown purges every active root's ephemeral store with zero leaks, and the daemon exits cleanly",
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

      const ownedDbFiles = () => readdirSync(runtimeDir).filter((f) => /\.db(?:-(?:wal|shm))?$/.test(f));
      expect(ownedDbFiles().filter((f) => f.endsWith(".db"))).toHaveLength(3);

      daemon.kill("SIGTERM");
      const exitResult = await waitForExit(daemon, 10_000);
      expect(exitResult).not.toBe("TIMEOUT");
      expect((exitResult as { code: number | null }).code).toBe(0);
      expect(ownedDbFiles()).toEqual([]);
    },
    30_000,
  );
});
