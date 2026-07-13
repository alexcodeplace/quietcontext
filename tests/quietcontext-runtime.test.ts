import { describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

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

describe("quietcontext MCP runtime", () => {
  test("batch execution returns compact output without inventories", async () => {
    const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-data-"));
    const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: {
        ...process.env,
        QUIET_CONTEXT_PLATFORM: "codex",
        QUIET_CONTEXT_DIR: dataDir,
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
      send(child, { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
      const toolsResponse = await readResponse(child);
      const toolNames = (toolsResponse.result?.tools ?? [])
        .map((tool: { name: string }) => tool.name)
        .sort();
      expect(toolNames).toEqual([
        "batch",
        "exec-file",
        "execute",
        "fetch-index",
        "index",
        "search",
      ]);
      send(child, {
        jsonrpc: "2.0",
        id: 3,
        method: "tools/call",
        params: {
          name: "execute",
          arguments: {
            language: "javascript",
            code: "console.log('quiet-execute')",
          },
        },
      });
      const executeResponse = await readResponse(child);
      expect(executeResponse.result?.content?.[0]?.text ?? "").toContain("quiet-execute");

      send(child, {
        jsonrpc: "2.0",
        id: 4,
        method: "tools/call",
        params: {
          name: "batch",
          arguments: {
            commands: [{ label: "quiet-check", command: "printf 'quiet\\n'" }],
            queries: ["quiet"],
          },
        },
      });

      const response = await readResponse(child);
      const text = response.result?.content?.[0]?.text ?? "";
      expect(text).toContain("1 command;");
      expect(text).not.toContain("## Commands");
      expect(text).not.toContain("## Indexed Sections");
      expect(text).not.toContain("Searchable terms");

      send(child, {
        jsonrpc: "2.0",
        id: 5,
        method: "tools/call",
        params: { name: "search", arguments: { queries: ["quiet"] } },
      });
      const searchResponse = await readResponse(child);
      const searchText = searchResponse.result?.content?.[0]?.text ?? "";
      expect(searchText).toContain("quiet-check");
      expect(searchText).not.toContain("## quiet");
      expect(searchText).not.toContain("###");
      expect(searchText).not.toContain("Throttle:");
    } finally {
      child.kill();
      rmSync(dataDir, { recursive: true, force: true });
    }
  }, 20_000);
});
