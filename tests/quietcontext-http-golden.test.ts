/**
 * HTTP-transport golden behavior for the six public tools.
 *
 * quietcontext-http.test.ts already pins auth, root validation, root
 * isolation, and execute/index/search over HTTP. This file fills the
 * remaining per-tool golden coverage the extraction lane needs: batch,
 * exec-file, fetch-index, script_ref reuse, and the response-budget caps
 * from docs/specs/2026-07-14-quietcontext-adaptive-token-controls-design.md
 * (execute/exec-file/search hard cap 8 KiB, batch hard cap 12 KiB,
 * preview adds at most 600 bytes per unique result), all through the real
 * launcher (start-http.mjs -> http-server.bundle.mjs), never by importing
 * src/ internals.
 */
import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createServer, type Server } from "node:http";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");

let daemon: ChildProcessWithoutNullStreams;
let port = 0;
let token = "";
let scratch = "";
let rootA = "";

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
  opts: { root?: string } = {},
): Promise<RpcResult> {
  return rpc("tools/call", { name, arguments: args }, { root: opts.root ?? rootA });
}

function toolText(r: RpcResult): string {
  expect(r.status, JSON.stringify(r.body)).toBe(200);
  expect(r.body?.error, JSON.stringify(r.body?.error)).toBeUndefined();
  const content = r.body?.result?.content ?? [];
  return content.map((c: { text?: string }) => c.text ?? "").join("\n");
}

const httpServers: Server[] = [];

beforeAll(async () => {
  scratch = mkdtempSync(join(tmpdir(), "qc-http-golden-"));
  rootA = join(scratch, "project-a");
  mkdirSync(rootA, { recursive: true });
  writeFileSync(join(rootA, "marker-a.txt"), "alpha-marker-content-8271\n");
  const tokenFile = join(scratch, "daemon.token");
  port = await startDaemon({
    QUIET_CONTEXT_DAEMON_PORT: "0",
    QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
    QUIET_CONTEXT_DIR: join(scratch, "data"),
  });
  const { readFileSync } = await import("node:fs");
  token = readFileSync(tokenFile, "utf8").trim();
}, 30_000);

afterAll(async () => {
  for (const server of httpServers.splice(0)) {
    await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
  }
  daemon?.kill("SIGTERM");
  rmSync(scratch, { recursive: true, force: true });
});

describe("quietcontext HTTP golden per-tool behavior", () => {
  test("tools/list over HTTP exposes exactly six canonical names with no ctx_ substring", async () => {
    const r = await rpc("tools/list", {});
    const tools = r.body?.result?.tools ?? [];
    expect(tools.map((t: { name: string }) => t.name).sort()).toEqual([
      "batch",
      "exec-file",
      "execute",
      "fetch-index",
      "index",
      "search",
    ]);
    expect(JSON.stringify(tools)).not.toMatch(/ctx_/);
  });

  test("batch runs a command, indexes it, and returns the compact golden shape", async () => {
    const r = await callTool("batch", {
      commands: [{ label: "probe-cmd", command: "printf 'probe-needle\\n'" }],
      queries: ["probe-needle"],
    });
    const text = toolText(r);
    expect(text).toBe(
      "1 command; 7 lines; 0.1KB; 1 sections indexed; 1 query.\n\n" +
        "query: probe-needle\n\nprobe-cmd\n# probe-cmd\n\n$ printf 'probe-needle\\n'\n\nprobe-needle",
    );
  });

  test("exec-file resolves a relative path against the caller's X-QuietContext-Root header", async () => {
    const r = await callTool("exec-file", {
      path: "marker-a.txt",
      language: "javascript",
      code: "console.log(FILE_CONTENT.trim())",
    });
    expect(toolText(r)).toBe("alpha-marker-content-8271\n");
  });

  test("execute and exec-file stay within the 8 KiB hard cap and index the overflow", async () => {
    const r = await callTool("execute", {
      language: "javascript",
      code: 'console.log("oversized-marker-" + "x".repeat(9000))',
    });
    const text = toolText(r);
    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(8 * 1024);
    expect(text).toContain("Indexed");
    expect(text).not.toContain("x".repeat(1000));
  });

  test("batch stays within the 12 KiB hard cap across duplicate queries", async () => {
    const command =
      "node -e \"for(let i=0;i<20;i++) console.log('# section'+i+'\\nneedle '+('q'.repeat(900)))\"";
    const r = await callTool("batch", {
      commands: [{ label: "budget-batch", command }],
      queries: Array.from({ length: 8 }, () => "needle"),
    });
    const text = toolText(r);
    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(12 * 1024);
  });

  test("execute reuses a long program through a short [s:<hex>] script reference", async () => {
    const code = `${"// reusable\n".repeat(30)}console.log("SCRIPT_OK")`;
    const first = toolText(await callTool("execute", { language: "javascript", code }));
    const ref = first.match(/\[s:([a-f0-9]+)\]/)?.[1];
    expect(ref).toBeTruthy();

    const second = toolText(await callTool("execute", { script_ref: `s:${ref}` }));
    expect(second).toContain("SCRIPT_OK");
  });

  test("fetch-index returns [source::url] metadata only, never the fetched page body", async () => {
    const body = "<html><body><h1>fetch-preview-must-stay-indexed</h1></body></html>";
    const server = createServer((_req, res) => {
      res.writeHead(200, { "content-type": "text/html" });
      res.end(body);
    });
    httpServers.push(server);
    await new Promise<void>((resolveListen) => server.listen(0, "127.0.0.1", () => resolveListen()));
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing test server address");

    const r = await callTool("fetch-index", {
      url: `http://127.0.0.1:${address.port}/fixture`,
      source: "fetch-fixture",
    });
    const text = toolText(r);
    expect(text).toContain("fetch-fixture");
    expect(text).not.toContain("fetch-preview-must-stay-indexed");
  });

  test("search returns [r:<id>] refs by default; preview adds at most 600 bytes per unique result", async () => {
    await callTool("index", {
      source: "reference-fixture",
      content: "# Alpha\n\nexact-reference-marker alpha beta\n\n" + "z".repeat(1200),
    });
    const plain = await callTool("search", { queries: ["alpha", "alpha beta"], limit: 3 });
    const plainText = toolText(plain);
    expect(plainText).not.toContain("exact-reference-marker");
    const refs = [...plainText.matchAll(/\[r:([a-z0-9]+)\]/gi)];
    expect(refs.length).toBeGreaterThan(0);

    const preview = await callTool("search", {
      queries: ["alpha", "alpha beta"],
      preview: true,
      limit: 3,
    });
    const previewText = toolText(preview);
    expect(previewText).toContain("exact-reference-marker");
    const delta = Buffer.byteLength(previewText) - Buffer.byteLength(plainText);
    expect(delta).toBeLessThanOrEqual(600 * refs.length);

    const exact = await callTool("search", { refs: [refs[0][1]] });
    expect(toolText(exact)).toContain("exact-reference-marker");
  });

  test("execute and search honor a lower max_bytes over HTTP", async () => {
    const r = await callTool("execute", {
      language: "javascript",
      code: 'console.log("budget-marker-" + "x".repeat(2000))',
      max_bytes: 512,
    });
    const text = toolText(r);
    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(512);
    expect(text).toContain("Indexed");
  });

  test("max_bytes outside 256..hard-cap is rejected with a clean error, not a crash", async () => {
    const r = await callTool("execute", {
      language: "javascript",
      code: "console.log('x'.repeat(50))",
      max_bytes: 10,
    });
    expect(r.status).toBe(200);
    expect(r.body?.result?.isError).toBe(true);
    expect(toolText(r)).toBe("max_bytes must be an integer from 256 through 8192.");
  });
});
