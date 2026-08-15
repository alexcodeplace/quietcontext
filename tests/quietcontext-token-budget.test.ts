import { afterEach, describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createServer, type Server } from "node:http";
import { mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");

interface Harness {
  child: ChildProcessWithoutNullStreams;
  dataDir: string;
  call(method: string, params: Record<string, unknown>): Promise<Record<string, any>>;
}

const harnesses: Harness[] = [];
const httpServers: Server[] = [];

async function startHarness(): Promise<Harness> {
  const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-budget-"));
  const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
    cwd: ROOT,
    env: {
      ...process.env,
      QUIET_CONTEXT_PLATFORM: "codex",
      QUIET_CONTEXT_DIR: dataDir,
    },
  });
  let nextId = 1;
  let buffer = "";
  const pending = new Map<number, {
    resolve: (value: Record<string, any>) => void;
    reject: (reason: unknown) => void;
  }>();

  child.stdout.on("data", (chunk: Buffer) => {
    buffer += chunk.toString();
    let newline = buffer.indexOf("\n");
    while (newline >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      newline = buffer.indexOf("\n");
      if (!line) continue;
      const response = JSON.parse(line) as Record<string, any>;
      const id = Number(response.id);
      const waiter = pending.get(id);
      if (!waiter) continue;
      pending.delete(id);
      waiter.resolve(response);
    }
  });
  child.once("error", (error) => {
    for (const waiter of pending.values()) waiter.reject(error);
    pending.clear();
  });

  const call = (method: string, params: Record<string, unknown>) => {
    const id = nextId++;
    const response = new Promise<Record<string, any>>((resolveResponse, reject) => {
      pending.set(id, { resolve: resolveResponse, reject });
    });
    child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    return response;
  };

  const harness = { child, dataDir, call };
  harnesses.push(harness);
  await call("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "quietcontext-budget-test", version: "1.0.0" },
  });
  child.stdin.write(JSON.stringify({
    jsonrpc: "2.0",
    method: "notifications/initialized",
    params: {},
  }) + "\n");
  return harness;
}

function resultText(response: Record<string, any>): string {
  if (response.error) throw new Error(JSON.stringify(response.error));
  return response.result?.content?.[0]?.text ?? "";
}

const httpDaemons: ChildProcessWithoutNullStreams[] = [];
const httpScratchDirs: string[] = [];

interface HttpHarness {
  port: number;
  token: string;
  root: string;
}

async function startHttpHarness(): Promise<HttpHarness> {
  const scratch = mkdtempSync(join(tmpdir(), "quietcontext-budget-http-"));
  httpScratchDirs.push(scratch);
  const root = join(scratch, "project-a");
  mkdirSync(root, { recursive: true });
  const tokenFile = join(scratch, "daemon.token");
  const port = await new Promise<number>((resolvePort, reject) => {
    const daemon = spawn(process.execPath, [join(ROOT, "start-http.mjs")], {
      cwd: ROOT,
      env: {
        ...process.env,
        QUIET_CONTEXT_DAEMON_PORT: "0",
        QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
        QUIET_CONTEXT_DIR: join(scratch, "data"),
      },
      stdio: ["ignore", "pipe", "pipe"],
    }) as ChildProcessWithoutNullStreams;
    httpDaemons.push(daemon);
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
  const token = readFileSync(tokenFile, "utf8").trim();
  return { port, token, root };
}

async function httpToolsList(harness: HttpHarness): Promise<Array<Record<string, unknown>>> {
  const res = await fetch(`http://127.0.0.1:${harness.port}/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      authorization: `Bearer ${harness.token}`,
      "x-quietcontext-root": harness.root,
    },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/list", params: {} }),
  });
  const text = await res.text();
  const dataLine = text.split("\n").find((l) => l.startsWith("data: "));
  const body = JSON.parse(dataLine ? dataLine.slice(6) : text);
  return body.result.tools;
}

afterEach(async () => {
  for (const server of httpServers.splice(0)) {
    await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
  }
  for (const harness of harnesses.splice(0)) {
    harness.child.kill();
    rmSync(harness.dataDir, { recursive: true, force: true });
  }
  for (const daemon of httpDaemons.splice(0)) {
    daemon.kill("SIGTERM");
  }
  for (const dir of httpScratchDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("QuietContext token budgets", () => {
  test("serialized tools/list stays within 4 KiB", async () => {
    const harness = await startHarness();
    const response = await harness.call("tools/list", {});
    const tools = response.result?.tools ?? [];

    expect(tools.map((tool: { name: string }) => tool.name).sort()).toEqual([
      "batch",
      "exec-file",
      "execute",
      "fetch-index",
      "index",
      "search",
    ]);
    expect(Buffer.byteLength(JSON.stringify(tools))).toBeLessThanOrEqual(4 * 1024);
  });

  test("serialized tools/list stays within 4 KiB over HTTP and matches stdio byte-for-byte", async () => {
    const stdioHarness = await startHarness();
    const stdioResponse = await stdioHarness.call("tools/list", {});
    const stdioTools = stdioResponse.result?.tools ?? [];

    const httpHarness = await startHttpHarness();
    const httpTools = await httpToolsList(httpHarness);

    expect(httpTools.map((tool: { name: string }) => tool.name).sort()).toEqual([
      "batch",
      "exec-file",
      "execute",
      "fetch-index",
      "index",
      "search",
    ]);
    expect(Buffer.byteLength(JSON.stringify(httpTools))).toBeLessThanOrEqual(4 * 1024);
    // Both transports register from the same REGISTERED_CTX_TOOLS list and
    // run the same strict-client schema pass — their wire schemas must be
    // byte-identical, not just each independently under budget.
    expect(JSON.stringify(httpTools)).toBe(JSON.stringify(stdioTools));
  });

  test("exec returns output without submitted source code", async () => {
    const harness = await startHarness();
    const marker = "submitted-source-must-not-return";
    const response = await harness.call("tools/call", {
      name: "execute",
      arguments: {
        language: "javascript",
        code: `// ${marker}\nconsole.log("OK")`,
      },
    });
    const text = resultText(response);

    expect(text.trim()).toBe("OK");
    expect(text).not.toContain(marker);
    expect(text).not.toContain("```javascript");
  });

  test("exec auto-indexes output above 8 KiB", async () => {
    const harness = await startHarness();
    const response = await harness.call("tools/call", {
      name: "execute",
      arguments: {
        language: "javascript",
        code: 'console.log("oversized-marker-" + "x".repeat(9000))',
      },
    });
    const text = resultText(response);

    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(8 * 1024);
    expect(text).toContain("Indexed");
    expect(text).not.toContain("x".repeat(1000));
  });

  test("search returns titles and refs by default, with opt-in previews", async () => {
    const harness = await startHarness();
    await harness.call("tools/call", {
      name: "index",
      arguments: {
        source: "reference-fixture",
        content: "# Alpha\n\nexact-reference-marker alpha beta\n\n" + "z".repeat(1200),
      },
    });
    const response = await harness.call("tools/call", {
      name: "search",
      arguments: { queries: ["alpha", "alpha beta"], limit: 3 },
    });
    const text = resultText(response);
    const ref = text.match(/\[r:([a-z0-9]+)\]/i)?.[1];

    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(8 * 1024);
    expect(text).not.toContain("exact-reference-marker");
    expect(ref).toBeTruthy();

    const preview = await harness.call("tools/call", {
      name: "search",
      arguments: { queries: ["alpha", "alpha beta"], preview: true, limit: 3 },
    });
    expect(resultText(preview).split("exact-reference-marker").length - 1).toBe(1);

    const exact = await harness.call("tools/call", {
      name: "search",
      arguments: { refs: [ref] },
    });
    expect(resultText(exact)).toContain("exact-reference-marker");
  });

  test("exec and exec-file honor lower max_bytes by indexing raw output", async () => {
    const harness = await startHarness();
    for (const [name, arguments_] of [
      ["execute", {
        language: "javascript",
        code: 'console.log("budget-marker-" + "x".repeat(2000))',
        max_bytes: 512,
      }],
      ["exec-file", {
        path: "README.md",
        language: "javascript",
        code: 'console.log("file-budget-marker-" + "x".repeat(2000))',
        max_bytes: 512,
      }],
    ] as const) {
      const response = await harness.call("tools/call", { name, arguments: arguments_ });
      const text = resultText(response);
      expect(Buffer.byteLength(text)).toBeLessThanOrEqual(512);
      expect(text).toContain("Indexed");
      expect(text).not.toContain("x".repeat(1000));
    }
  });

  test("search and batch honor lower max_bytes", async () => {
    const harness = await startHarness();
    await harness.call("tools/call", {
      name: "index",
      arguments: {
        source: "lower-budget-fixture",
        content: "# Budget\n\nlower-budget-marker " + "z".repeat(2000),
      },
    });
    const searched = await harness.call("tools/call", {
      name: "search",
      arguments: { queries: ["lower-budget-marker"], preview: true, max_bytes: 256 },
    });
    expect(Buffer.byteLength(resultText(searched))).toBeLessThanOrEqual(256);

    const batched = await harness.call("tools/call", {
      name: "batch",
      arguments: {
        commands: [{ label: "lower-budget", command: "node -e \"console.log('needle '+('q'.repeat(2000)))\"" }],
        queries: ["needle"],
        max_bytes: 512,
      },
    });
    expect(Buffer.byteLength(resultText(batched))).toBeLessThanOrEqual(512);
  });

  test("exec reuses long source through a short script reference", async () => {
    const harness = await startHarness();
    const code = `${"// reusable\n".repeat(30)}console.log("SCRIPT_OK")`;
    const first = resultText(await harness.call("tools/call", {
      name: "execute",
      arguments: { language: "javascript", code },
    }));
    const ref = first.match(/\[s:([a-f0-9]+)\]/)?.[1];
    expect(ref).toBeTruthy();

    const second = resultText(await harness.call("tools/call", {
      name: "execute",
      arguments: { script_ref: `s:${ref}` },
    }));
    expect(second).toContain("SCRIPT_OK");
  });

  test("exec-file reuses one cached program across files", async () => {
    const harness = await startHarness();
    const code = `${"// reusable-file\n".repeat(30)}console.log(FILE_CONTENT.length > 0 ? "FILE_OK" : "BAD")`;
    const first = resultText(await harness.call("tools/call", {
      name: "exec-file",
      arguments: { path: "README.md", language: "javascript", code },
    }));
    const ref = first.match(/\[s:([a-f0-9]+)\]/)?.[1];
    expect(ref).toBeTruthy();

    const second = resultText(await harness.call("tools/call", {
      name: "exec-file",
      arguments: { path: "package.json", script_ref: ref },
    }));
    expect(second).toContain("FILE_OK");
  });

  test("exec rejects unknown and ambiguous script references", async () => {
    const harness = await startHarness();
    const unknown = await harness.call("tools/call", {
      name: "execute",
      arguments: { script_ref: "deadbeef" },
    });
    expect(resultText(unknown)).toContain("Unknown script_ref");

    const ambiguous = await harness.call("tools/call", {
      name: "execute",
      arguments: {
        language: "javascript",
        code: "console.log('no')",
        script_ref: "deadbeef",
      },
    });
    expect(resultText(ambiguous)).toContain("Provide code or script_ref, not both");
  });

  test("batch response stays within 12 KiB across duplicate queries", async () => {
    const harness = await startHarness();
    const command = "node -e \"for(let i=0;i<20;i++) console.log('# section'+i+'\\nneedle '+('q'.repeat(900)))\"";
    const response = await harness.call("tools/call", {
      name: "batch",
      arguments: {
        commands: [{ label: "budget-batch", command }],
        queries: Array.from({ length: 8 }, () => "needle"),
      },
    });
    const text = resultText(response);

    expect(Buffer.byteLength(text)).toBeLessThanOrEqual(12 * 1024);
    expect(text.split("needle").length - 1).toBeLessThanOrEqual(3);
  });

  test("fetch-index returns metadata without page preview", async () => {
    const body = "<html><body><h1>fetch-preview-must-stay-indexed</h1></body></html>";
    const server = createServer((_request, response) => {
      response.writeHead(200, { "content-type": "text/html" });
      response.end(body);
    });
    httpServers.push(server);
    await new Promise<void>((resolveListen) => {
      server.listen(0, "127.0.0.1", () => resolveListen());
    });
    const address = server.address();
    if (!address || typeof address === "string") throw new Error("missing test server address");

    const harness = await startHarness();
    const response = await harness.call("tools/call", {
      name: "fetch-index",
      arguments: { url: `http://127.0.0.1:${address.port}/fixture`, source: "fetch-fixture" },
    });
    const text = resultText(response);

    expect(text).toContain("fetch-fixture");
    expect(text).not.toContain("fetch-preview-must-stay-indexed");
  });
});
