import { describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, basename, resolve } from "node:path";
import { createProcessStorePath } from "../src/store.js";

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

function waitForExit(child: ChildProcessWithoutNullStreams): Promise<void> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolveExit, reject) => {
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error("QuietContext server did not exit after stdin close"));
    }, 10_000);
    child.once("exit", () => {
      clearTimeout(timer);
      resolveExit();
    });
  });
}

describe("quietcontext MCP runtime", () => {
  test("creates unique store paths for the same process instance", () => {
    const paths = Array.from({ length: 32 }, () => createProcessStorePath());
    expect(new Set(paths).size).toBe(paths.length);
    for (const path of paths) {
      expect(basename(path)).toMatch(new RegExp(`^context-mode-${process.pid}-[a-f0-9]{16}\\.db$`));
    }
  });

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

  test("isolates concurrent PID-owned stores and cleans only their own files", async () => {
    const storageDir = mkdtempSync(join(tmpdir(), "quietcontext-storage-"));
    const runtimeDir = mkdtempSync(join(tmpdir(), "quietcontext-runtime-"));
    const sharedContentDir = join(storageDir, "content");
    const staleSharedDb = join(sharedContentDir, "stale-shared.db");
    const environment = {
      ...process.env,
      QUIET_CONTEXT_PLATFORM: "codex",
      QUIET_CONTEXT_DIR: storageDir,
      TMPDIR: runtimeDir,
      TEMP: runtimeDir,
      TMP: runtimeDir,
    };
    const first = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: environment,
    });
    const second = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: environment,
    });

    const initialize = async (child: ChildProcessWithoutNullStreams, name: string) => {
      send(child, {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name, version: "1.0.0" },
        },
      });
      await readResponse(child);
      send(child, { jsonrpc: "2.0", method: "notifications/initialized", params: {} });
    };
    const callTool = async (
      child: ChildProcessWithoutNullStreams,
      id: number,
      name: string,
      args: Record<string, unknown>,
    ) => {
      send(child, {
        jsonrpc: "2.0",
        id,
        method: "tools/call",
        params: { name, arguments: args },
      });
      return readResponse(child);
    };
    const ownedDbFiles = () => readdirSync(runtimeDir)
      .filter((file) => /^context-mode-\d+-[a-f0-9]{16}\.db(?:-(?:wal|shm))?$/.test(file));

    try {
      mkdirSync(sharedContentDir, { recursive: true });
      writeFileSync(staleSharedDb, "stale shared content database");
      const staleTime = new Date(Date.now() - 15 * 24 * 60 * 60 * 1000);
      utimesSync(staleSharedDb, staleTime, staleTime);

      await Promise.all([
        initialize(first, "quietcontext-store-test-first"),
        initialize(second, "quietcontext-store-test-second"),
      ]);

      const [firstIndex, secondIndex] = await Promise.all([
        callTool(first, 2, "index", {
          source: "first-pid-fixture",
          content: "# First process\n\nzephyralpha731",
        }),
        callTool(second, 2, "index", {
          source: "second-pid-fixture",
          content: "# Second process\n\nquartzomega928",
        }),
      ]);
      expect(firstIndex.result?.isError).not.toBe(true);
      expect(secondIndex.result?.isError).not.toBe(true);

      const [[firstOwn, firstOther], [secondOwn, secondOther]] = await Promise.all([
        (async () => [
          await callTool(first, 3, "search", { queries: ["zephyralpha731"] }),
          await callTool(first, 4, "search", { queries: ["quartzomega928"] }),
        ])(),
        (async () => [
          await callTool(second, 3, "search", { queries: ["quartzomega928"] }),
          await callTool(second, 4, "search", { queries: ["zephyralpha731"] }),
        ])(),
      ]);
      expect(firstOwn.result?.content?.[0]?.text ?? "").not.toContain("No results found.");
      expect(firstOther.result?.content?.[0]?.text ?? "").toContain("No results found.");
      expect(secondOwn.result?.content?.[0]?.text ?? "").not.toContain("No results found.");
      expect(secondOther.result?.content?.[0]?.text ?? "").toContain("No results found.");

      expect(ownedDbFiles().filter((file) => file.endsWith(".db"))).toHaveLength(2);
      expect(existsSync(staleSharedDb)).toBe(true);

      first.stdin.end();
      await waitForExit(first);
      expect(ownedDbFiles().filter((file) => file.endsWith(".db"))).toHaveLength(1);
      expect(existsSync(staleSharedDb)).toBe(true);

      const secondAfterFirstShutdown = await callTool(second, 5, "search", {
        queries: ["quartzomega928"],
      });
      expect(secondAfterFirstShutdown.result?.content?.[0]?.text ?? "").toContain("quartzomega928");

      second.stdin.end();
      await waitForExit(second);
      expect(ownedDbFiles()).toEqual([]);
      expect(existsSync(staleSharedDb)).toBe(true);
    } finally {
      if (first.exitCode === null && first.signalCode === null) first.stdin.end();
      if (second.exitCode === null && second.signalCode === null) second.stdin.end();
      await Promise.all([waitForExit(first), waitForExit(second)]);
      rmSync(storageDir, { recursive: true, force: true });
      rmSync(runtimeDir, { recursive: true, force: true });
    }
  }, 30_000);
});
