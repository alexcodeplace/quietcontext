import { afterAll, beforeAll, describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { qcNativePlatformKey } from "../src/native-qc.js";

const ROOT = resolve(import.meta.dirname, "..");
const key = qcNativePlatformKey();
const nativePresent = !!key && existsSync(join(ROOT, "vendor", "qc-native", key, process.platform === "win32" ? "qc-native.exe" : "qc-native"));
const suite = nativePresent ? describe : describe.skip;

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
      const match = /listening on http:\/\/127\.0\.0\.1:(\d+)\/mcp/.exec(stderr);
      if (match) {
        daemon.stderr.off("data", onData);
        resolvePort(Number(match[1]));
      }
    };
    daemon.stderr.on("data", onData);
    daemon.once("exit", (code) => reject(new Error(`daemon exited early (${code}): ${stderr}`)));
    daemon.once("error", reject);
  });
}

let nextId = 1;
async function callTool(name: string, args: Record<string, unknown>, root: string): Promise<any> {
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
  const dataLine = text.split("\n").find((line) => line.startsWith("data: "));
  const body = JSON.parse(dataLine ? dataLine.slice(6) : text);
  expect(res.status, JSON.stringify(body)).toBe(200);
  expect(body.error, JSON.stringify(body.error)).toBeUndefined();
  return body.result;
}

function text(result: any): string {
  return (result?.content ?? []).map((item: { text?: string }) => item.text ?? "").join("\n");
}

beforeAll(async () => {
  if (!nativePresent) return;
  scratch = mkdtempSync(join(tmpdir(), "qc-native-http-"));
  rootA = join(scratch, "a");
  rootB = join(scratch, "b");
  mkdirSync(join(rootA, "src"), { recursive: true });
  mkdirSync(join(rootB, "src"), { recursive: true });
  writeFileSync(join(rootA, "src", "a.ts"), "export function alphaOnly() { return 1; }\nexport const a = alphaOnly();\n");
  writeFileSync(join(rootB, "src", "b.ts"), "export function betaOnly() { return 2; }\nexport const b = betaOnly();\n");
  const lines = Array.from({ length: 180 }, (_, i) => i === 149 ? "RAW_RECOVERY_CANARY_f3e77" : `ordinary line ${i + 1}`);
  writeFileSync(join(rootA, "large.log"), lines.join("\n") + "\n");
  const tokenFile = join(scratch, "daemon.token");
  port = await startDaemon({
    HOME: scratch,
    CLAUDE_CONFIG_DIR: join(scratch, "claude"),
    QUIET_CONTEXT_PLATFORM: "claude-code",
    QUIET_CONTEXT_DAEMON_PORT: "0",
    QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
    QUIET_CONTEXT_STORAGE_ROOT: join(scratch, "storage"),
    XDG_STATE_HOME: join(scratch, "state"),
  });
  token = readFileSync(tokenFile, "utf8").trim();
}, 30_000);

afterAll(() => {
  daemon?.kill("SIGTERM");
  if (scratch) {
    try {
      const pid = Number(readFileSync(join(scratch, "state", "quietcontext", "native", "repomap", "repomap-v2.pid"), "utf8").trim());
      if (Number.isInteger(pid) && pid > 1) process.kill(pid, "SIGTERM");
    } catch { /* no daemon or already stopped */ }
    rmSync(scratch, { recursive: true, force: true });
  }
});

suite("qc native HTTP integration", () => {
  test("repo map is isolated between concurrent project roots", async () => {
    const [a, b] = await Promise.all([
      callTool("repo", { action: "map" }, rootA),
      callTool("repo", { action: "map" }, rootB),
    ]);
    expect(text(a)).toContain("alphaOnly");
    expect(text(a)).not.toContain("betaOnly");
    expect(text(b)).toContain("betaOnly");
    expect(text(b)).not.toContain("alphaOnly");
  });

  test("symbol, references and outline use the same native index", async () => {
    expect(text(await callTool("repo", { action: "symbol", target: "alphaOnly" }, rootA))).toContain("src/a.ts");
    expect(text(await callTool("repo", { action: "references", target: "alphaOnly" }, rootA))).toContain("alphaOnly()");
    expect(text(await callTool("repo", { action: "outline", target: "src/a.ts" }, rootA))).toContain("alphaOnly");
  });

  test("execute uses native filtering and search recovers an omitted raw canary", async () => {
    const executed = text(await callTool("execute", { language: "shell", code: "cat large.log" }, rootA));
    expect(executed).toContain("[showing first 100 of 180 lines]");
    expect(executed).not.toContain("RAW_RECOVERY_CANARY_f3e77");
    expect(executed).toContain("[qc evidence:");
    expect(executed).toContain("search recovers omitted raw output");

    const searched = text(await callTool("search", { queries: ["RAW_RECOVERY_CANARY_f3e77"] }, rootA));
    expect(searched).toContain("RAW_RECOVERY_CANARY_f3e77");
  });
});
