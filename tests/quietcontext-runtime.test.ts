import { describe, expect, test } from "vitest";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, basename, resolve } from "node:path";
import { createProcessStorePath } from "../src/store.js";
import { loadDatabase } from "../src/db-base.js";
import {
  getStore,
  rememberSearchReference,
  resolveSearchReference,
  withProjectDirOverride,
} from "../src/server.js";
import { FloodGuard } from "../src/search/flood-guard.js";
import {
  buildCtxSearchInputSchema,
  MAX_SEARCH_QUERIES,
  MAX_SEARCH_QUERY_BYTES,
} from "../src/search/ctx-search-schema.js";

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

  test("bounds synchronous search work at the schema boundary", () => {
    const schema = buildCtxSearchInputSchema();
    expect(() => schema.parse({ queries: Array(MAX_SEARCH_QUERIES + 1).fill("miss") }))
      .toThrow();
    expect(() => schema.parse({ queries: ["é".repeat(MAX_SEARCH_QUERY_BYTES)] }))
      .toThrow();
    expect(schema.parse({ queries: Array(MAX_SEARCH_QUERIES).fill("bounded") }).queries)
      .toHaveLength(MAX_SEARCH_QUERIES);
    const guard = new FloodGuard({ windowMs: 60_000, softCapAfter: 3, blockAfter: 8 });
    expect(guard.record("actor", 1, MAX_SEARCH_QUERIES).count).toBe(MAX_SEARCH_QUERIES);
    expect(guard.record("actor", 2).blocked).toBe(true);
  });

  test("bounds and namespaces opaque search references", () => {
    const first = rememberSearchReference(
      { title: "first", source: "source", content: "secret" },
      "store-a\0session-a",
    );
    expect(first).toMatch(/^[A-Za-z0-9_-]{16}$/);
    expect(resolveSearchReference(first, "store-a\0session-b")).toBeUndefined();
    expect(resolveSearchReference(first, "store-b\0session-a")).toBeUndefined();
    for (let index = 0; index < 512; index++) {
      rememberSearchReference(
        { title: `title-${index}`, source: "source", content: `content-${index}` },
        "store-a\0session-a",
      );
    }
    expect(resolveSearchReference(first, "store-a\0session-a")).toBeUndefined();
  });

  test("charges search-reference retrieval against the flood budget", async () => {
    const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-ref-flood-"));
    const child = spawn(process.execPath, [join(ROOT, "start.mjs")], {
      cwd: ROOT,
      env: {
        ...process.env,
        QUIET_CONTEXT_PLATFORM: "codex",
        QUIET_CONTEXT_DIR: dataDir,
        QUIET_CONTEXT_SEARCH_MAX_RESULTS_AFTER: "1",
        QUIET_CONTEXT_SEARCH_BLOCK_AFTER: "1",
      },
    });
    const callTool = async (id: number, name: string, args: Record<string, unknown>) => {
      send(child, {
        jsonrpc: "2.0",
        id,
        method: "tools/call",
        params: { name, arguments: args },
      });
      return readResponse(child);
    };

    try {
      send(child, {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2024-11-05",
          capabilities: {},
          clientInfo: { name: "quietcontext-ref-flood", version: "1.0.0" },
        },
      });
      await readResponse(child);
      send(child, { jsonrpc: "2.0", method: "notifications/initialized", params: {} });
      await callTool(2, "index", {
        source: "reference-flood-source",
        content: "referencefloodsentinel",
      });
      const initial = await callTool(3, "search", { queries: ["referencefloodsentinel"] });
      const initialText = initial.result?.content?.[0]?.text ?? "";
      const reference = initialText.match(/\[r:([A-Za-z0-9_-]{16})\]/)?.[1];
      expect(reference).toBeDefined();

      const blocked = await callTool(4, "search", { refs: [reference] });
      expect(blocked.result?.isError).toBe(true);
      expect(blocked.result?.content?.[0]?.text ?? "").toContain("BLOCKED:");
    } finally {
      child.kill();
      rmSync(dataDir, { recursive: true, force: true });
    }
  }, 20_000);

  test("isolates persistent stores between in-process project contexts", async () => {
    const storageDir = mkdtempSync(join(tmpdir(), "quietcontext-project-stores-"));
    const previousDir = process.env.QUIET_CONTEXT_DIR;
    process.env.QUIET_CONTEXT_DIR = storageDir;
    let storeA: ReturnType<typeof getStore> | undefined;
    let storeB: ReturnType<typeof getStore> | undefined;
    try {
      storeA = await withProjectDirOverride(join(storageDir, "project-a"), async () => getStore());
      storeB = await withProjectDirOverride(join(storageDir, "project-b"), async () => getStore());
      expect(storeA).not.toBe(storeB);
      storeA.index({ content: "alpha project sentinel", source: "alpha-source" });
      storeB.index({ content: "beta project sentinel", source: "beta-source" });
      expect(storeA.search("beta sentinel")).toHaveLength(0);
      expect(storeB.search("alpha sentinel")).toHaveLength(0);
    } finally {
      storeA?.cleanup();
      storeB?.cleanup();
      if (previousDir === undefined) delete process.env.QUIET_CONTEXT_DIR;
      else process.env.QUIET_CONTEXT_DIR = previousDir;
      rmSync(storageDir, { recursive: true, force: true });
    }
  });

  test("bounds in-process project store handles with LRU eviction", async () => {
    const storageDir = mkdtempSync(join(tmpdir(), "quietcontext-store-lru-"));
    const previousDir = process.env.QUIET_CONTEXT_DIR;
    process.env.QUIET_CONTEXT_DIR = storageDir;
    const opened: Array<ReturnType<typeof getStore>> = [];
    try {
      for (let index = 0; index < 17; index++) {
        opened.push(await withProjectDirOverride(
          join(storageDir, `project-${index}`),
          async () => getStore(),
        ));
      }
      expect(() => opened[0].getStats()).toThrow();
      expect(opened[16].getStats()).toEqual({ sources: 0, chunks: 0, codeChunks: 0 });
    } finally {
      for (const store of opened) store.cleanup();
      if (previousDir === undefined) delete process.env.QUIET_CONTEXT_DIR;
      else process.env.QUIET_CONTEXT_DIR = previousDir;
      rmSync(storageDir, { recursive: true, force: true });
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

  test("shares one canonical persistent store across concurrent MCP processes", async () => {
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
      expect(firstIndex.result?.isError, JSON.stringify(firstIndex)).not.toBe(true);
      expect(secondIndex.result?.isError, JSON.stringify(secondIndex)).not.toBe(true);

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
      expect(firstOther.result?.content?.[0]?.text ?? "").not.toContain("No results found.");
      expect(secondOwn.result?.content?.[0]?.text ?? "").not.toContain("No results found.");
      expect(secondOther.result?.content?.[0]?.text ?? "").not.toContain("No results found.");

      expect(ownedDbFiles()).toEqual([]);
      expect(readdirSync(sharedContentDir).filter((file) => file.endsWith(".db") && file !== "stale-shared.db")).toHaveLength(1);
      expect(existsSync(staleSharedDb)).toBe(true);

      const shutdownStartedAt = Date.now();
      first.stdin.end();
      await waitForExit(first);
      expect(Date.now() - shutdownStartedAt).toBeLessThan(1_000);
      expect(ownedDbFiles()).toEqual([]);
      expect(existsSync(staleSharedDb)).toBe(true);

      const secondAfterFirstShutdown = await callTool(second, 5, "search", {
        queries: ["quartzomega928"],
      });
      expect(secondAfterFirstShutdown.result?.content?.[0]?.text ?? "").toContain("quartzomega928");

      second.stdin.end();
      await waitForExit(second);
      expect(ownedDbFiles()).toEqual([]);
      const canonicalFiles = readdirSync(sharedContentDir)
        .filter((file) => file.endsWith(".db") && file !== "stale-shared.db");
      expect(canonicalFiles).toHaveLength(1);
      const Database = loadDatabase();
      const inspect = new Database(join(sharedContentDir, canonicalFiles[0]), { readonly: true });
      const durable = inspect.prepare(
        "SELECT indexed_bytes, chunks, sources FROM store_totals WHERE singleton = 1",
      ).get() as Record<string, number>;
      const actual = inspect.prepare(
        "SELECT COALESCE(SUM(indexed_bytes), 0) AS indexed_bytes, COALESCE(SUM(chunk_count), 0) AS chunks, COUNT(*) AS sources FROM sources",
      ).get() as Record<string, number>;
      expect(durable).toEqual(actual);
      inspect.close();
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
