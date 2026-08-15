/**
 * Shared HTTP daemon contract: one daemon process, many concurrent clients,
 * per-request working-root isolation, bearer-token auth.
 *
 * Spawns the REAL launcher (start-http.mjs → http-server.bundle.mjs), so the
 * artifact that systemd runs is the artifact under test.
 */
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");

let daemon: ChildProcessWithoutNullStreams;
let port = 0;
let token = "";
let scratch = "";
let rootA = "";
let rootB = "";

function startDaemon(env: Record<string, string>): Promise<number> {
  return new Promise((resolvePort, reject) => {
    daemon = spawn(process.execPath, [join(ROOT, "start-http.mjs")], {
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
        resolvePort(Number(m[1]));
      }
    };
    daemon.stderr.on("data", onData);
    daemon.once("exit", (code) => reject(new Error(`daemon exited early (${code}): ${stderr}`)));
    daemon.once("error", reject);
  });
}

interface RpcResult {
  status: number;
  body: any;
}

let nextId = 1;
async function rpc(
  method: string,
  params: Record<string, unknown>,
  opts: { root?: string; auth?: string | null } = {},
): Promise<RpcResult> {
  const headers: Record<string, string> = {
    "content-type": "application/json",
    accept: "application/json, text/event-stream",
  };
  const auth = opts.auth === undefined ? `Bearer ${token}` : opts.auth;
  if (auth !== null) headers.authorization = auth;
  if (opts.root) headers["x-quietcontext-root"] = opts.root;
  const res = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST",
    headers,
    body: JSON.stringify({ jsonrpc: "2.0", id: nextId++, method, params }),
  });
  const text = await res.text();
  // Stateless streamable HTTP replies as SSE or plain JSON depending on accept.
  let body: unknown = null;
  const dataLine = text.split("\n").find((l) => l.startsWith("data: "));
  try {
    body = JSON.parse(dataLine ? dataLine.slice(6) : text);
  } catch {
    body = text;
  }
  return { status: res.status, body };
}

function callTool(
  name: string,
  args: Record<string, unknown>,
  opts: { root?: string; auth?: string | null } = {},
): Promise<RpcResult> {
  return rpc("tools/call", { name, arguments: args }, opts);
}

function toolText(r: RpcResult): string {
  expect(r.status, JSON.stringify(r.body)).toBe(200);
  expect(r.body?.error, JSON.stringify(r.body?.error)).toBeUndefined();
  const content = r.body?.result?.content ?? [];
  return content.map((c: { text?: string }) => c.text ?? "").join("\n");
}

beforeAll(async () => {
  scratch = mkdtempSync(join(tmpdir(), "qc-http-test-"));
  rootA = join(scratch, "project-a");
  rootB = join(scratch, "project-b");
  for (const dir of [rootA, rootB]) {
    // eslint-disable-next-line @typescript-eslint/no-require-imports
    (require("node:fs") as typeof import("node:fs")).mkdirSync(dir, { recursive: true });
  }
  writeFileSync(join(rootA, "marker-a.txt"), "alpha-marker-content-8271\n");
  writeFileSync(join(rootB, "marker-b.txt"), "beta-marker-content-9382\n");
  const tokenFile = join(scratch, "daemon.token");
  port = await startDaemon({
    QUIET_CONTEXT_DAEMON_PORT: "0",
    QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
  });
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  token = (require("node:fs") as typeof import("node:fs"))
    .readFileSync(tokenFile, "utf8")
    .trim();
}, 30_000);

afterAll(() => {
  daemon?.kill("SIGTERM");
  rmSync(scratch, { recursive: true, force: true });
});

describe("quietcontext shared HTTP daemon", () => {
  test("healthz answers without auth", async () => {
    const res = await fetch(`http://127.0.0.1:${port}/healthz`);
    expect(res.status).toBe(200);
    const body = await res.json();
    expect(body.ok).toBe(true);
    expect(body.name).toBe("quietcontext");
  });

  test("rejects missing and wrong bearer tokens", async () => {
    const missing = await rpc("tools/list", {}, { auth: null });
    expect(missing.status).toBe(401);
    const wrong = await rpc("tools/list", {}, { auth: "Bearer definitely-wrong" });
    expect(wrong.status).toBe(401);
  });

  test("exposes exactly the six public quiet tools", async () => {
    const r = await rpc("tools/list", {});
    expect(r.status).toBe(200);
    const names = (r.body?.result?.tools ?? []).map((t: { name: string }) => t.name).sort();
    expect(names).toEqual(["batch", "exec-file", "execute", "fetch-index", "index", "search"]);
  });

  test("tools/call without a working-root header is rejected", async () => {
    const r = await callTool("execute", { language: "shell", code: "pwd" });
    expect(r.status).toBe(400);
    expect(JSON.stringify(r.body)).toContain("x-quietcontext-root");
  });

  test("tools/call with a relative or bogus root is rejected", async () => {
    const rel = await callTool(
      "execute",
      { language: "shell", code: "pwd" },
      { root: "not/absolute" },
    );
    expect(rel.status).toBe(400);
    const gone = await callTool(
      "execute",
      { language: "shell", code: "pwd" },
      { root: join(scratch, "no-such-dir") },
    );
    expect(gone.status).toBe(400);
  });

  test("two concurrent clients run against their own working roots", async () => {
    const interleaved = await Promise.all([
      callTool("execute", { language: "shell", code: "pwd && cat marker-a.txt" }, { root: rootA }),
      callTool("execute", { language: "shell", code: "pwd && cat marker-b.txt" }, { root: rootB }),
      callTool("execute", { language: "shell", code: "pwd" }, { root: rootA }),
      callTool("execute", { language: "shell", code: "pwd" }, { root: rootB }),
    ]);
    const [a1, b1, a2, b2] = interleaved.map(toolText);
    expect(a1).toContain(rootA);
    expect(a1).toContain("alpha-marker-content-8271");
    expect(b1).toContain(rootB);
    expect(b1).toContain("beta-marker-content-9382");
    expect(a2).toContain(rootA);
    expect(a2).not.toContain(rootB);
    expect(b2).toContain(rootB);
  }, 30_000);

  test("index/search stores are isolated per working root", async () => {
    const idx = await callTool(
      "index",
      { content: "the launch code is zebra-pineapple-4417", source: "secret-a" },
      { root: rootA },
    );
    toolText(idx);

    const hitA = await callTool(
      "search",
      { queries: ["zebra-pineapple"], preview: true },
      { root: rootA },
    );
    expect(toolText(hitA)).toContain("zebra-pineapple");

    const missB = await callTool("search", { queries: ["zebra-pineapple"] }, { root: rootB });
    const missText = toolText(missB);
    expect(missText).not.toContain("zebra-pineapple-4417");
    expect(missText).not.toMatch(/\[r:\d+\]/);
  }, 30_000);
});
