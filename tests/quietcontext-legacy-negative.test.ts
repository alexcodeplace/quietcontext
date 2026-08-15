/**
 * Legacy tool name invocation must fail closed with a clean, in-band
 * MCP tool error over BOTH transports — never a protocol-level crash or
 * a dead child/daemon process. Complements the config-text greps in
 * quietcontext-surface.test.ts, which never actually invoke a tool.
 */
import { describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dirname, "..");
const LEGACY_TOOL_ERROR = "MCP error -32602: Tool ctx_execute not found";

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

describe("legacy tool name over stdio", () => {
  test("ctx_execute fails closed as an in-band tool error; the process stays alive", async () => {
    const dataDir = mkdtempSync(join(tmpdir(), "qc-legacy-stdio-"));
    const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: { ...process.env, QUIET_CONTEXT_PLATFORM: "codex", QUIET_CONTEXT_DIR: dataDir },
    });
    try {
      send(child, {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "quietcontext-legacy-test", version: "1.0.0" },
        },
      });
      await readResponse(child);
      send(child, { jsonrpc: "2.0", method: "notifications/initialized", params: {} });
      send(child, {
        jsonrpc: "2.0",
        id: 2,
        method: "tools/call",
        params: { name: "ctx_execute", arguments: { language: "shell", code: "echo hi" } },
      });
      const response = await readResponse(child);
      expect(response.result?.isError).toBe(true);
      expect(response.result?.content?.[0]?.text).toBe(LEGACY_TOOL_ERROR);
      expect(child.exitCode).toBeNull();
      expect(child.signalCode).toBeNull();
    } finally {
      child.kill();
      rmSync(dataDir, { recursive: true, force: true });
    }
  }, 20_000);
});

describe("legacy tool name over HTTP", () => {
  test("ctx_execute fails closed as an in-band tool error, distinct from the missing-root 400", async () => {
    const scratch = mkdtempSync(join(tmpdir(), "qc-legacy-http-"));
    const rootA = join(scratch, "project-a");
    mkdirSync(rootA, { recursive: true });
    const tokenFile = join(scratch, "daemon.token");

    let daemon: ChildProcessWithoutNullStreams | undefined;
    try {
      const port = await new Promise<number>((resolvePort, reject) => {
        daemon = spawn(process.execPath, [join(ROOT, "start-http.mjs")], {
          cwd: ROOT,
          env: {
            ...process.env,
            QUIET_CONTEXT_DAEMON_PORT: "0",
            QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
            QUIET_CONTEXT_DIR: join(scratch, "data"),
          },
          stdio: ["ignore", "pipe", "pipe"],
        }) as ChildProcessWithoutNullStreams;
        let stderr = "";
        const onData = (chunk: Buffer) => {
          stderr += chunk.toString();
          const m = /listening on http:\/\/127\.0\.0\.1:(\d+)\/mcp/.exec(stderr);
          if (m) {
            daemon!.stderr.off("data", onData);
            resolvePort(Number(m[1]));
          }
        };
        daemon.stderr.on("data", onData);
        daemon.once("exit", (code) => reject(new Error(`daemon exited early (${code}): ${stderr}`)));
        daemon.once("error", reject);
      });
      const token = readFileSync(tokenFile, "utf8").trim();

      const call = (root: string | undefined) =>
        fetch(`http://127.0.0.1:${port}/mcp`, {
          method: "POST",
          headers: {
            "content-type": "application/json",
            accept: "application/json, text/event-stream",
            authorization: `Bearer ${token}`,
            ...(root ? { "x-quietcontext-root": root } : {}),
          },
          body: JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "tools/call",
            params: { name: "ctx_execute", arguments: { language: "shell", code: "echo hi" } },
          }),
        });

      // Missing root is checked before the tool name is resolved: 400, not a tool error.
      const withoutRoot = await call(undefined);
      expect(withoutRoot.status).toBe(400);
      const withoutRootBody = await withoutRoot.json();
      expect(withoutRootBody.error?.message).toContain("x-quietcontext-root");

      const withRoot = await call(rootA);
      expect(withRoot.status).toBe(200);
      const text = await withRoot.text();
      const dataLine = text.split("\n").find((l) => l.startsWith("data: "));
      const withRootBody = JSON.parse(dataLine ? dataLine.slice(6) : text);
      expect(withRootBody.result?.isError).toBe(true);
      expect(withRootBody.result?.content?.[0]?.text).toBe(LEGACY_TOOL_ERROR);
    } finally {
      daemon?.kill("SIGTERM");
      rmSync(scratch, { recursive: true, force: true });
    }
  }, 30_000);
});
