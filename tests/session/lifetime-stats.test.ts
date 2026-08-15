/**
 * lifetime-stats — Bug #3 + #4
 *
 * Bug #3: Persistent memory totals (events across all sessions, not just
 * the current one) must be visible in ctx_stats so the user sees the
 * cumulative value of context-mode.
 *
 * Bug #4: Auto-memory captured by Claude Code under
 * ~/.claude/projects/<project>/memory/*.md is invisible today. ctx_stats
 * should surface the count and the projects involved.
 */

import { join } from "node:path";
import { tmpdir } from "node:os";
import { randomUUID } from "node:crypto";
import { mkdirSync, writeFileSync, rmSync } from "node:fs";
import { afterAll, describe, expect, test } from "vitest";
import { SessionDB } from "../../src/session/db.js";
import { BunSQLiteAdapter } from "../../src/db-base.js";
import { getContentBytesAllSessions, getContentBytesForSession, getConversationStats, getConversationWindowStats, getLifetimeStats, getMultiAdapterLifetimeStats, getMultiAdapterRealBytesStats } from "../../src/session/analytics.js";

const cleanups: Array<() => void> = [];

afterAll(() => {
  for (const fn of cleanups) {
    try { fn(); } catch { /* ignore */ }
  }
});

function tmpDir(prefix: string): string {
  const dir = join(tmpdir(), `${prefix}-${randomUUID()}`);
  mkdirSync(dir, { recursive: true });
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function tmpDbPath(dir: string, name: string): string {
  return join(dir, `${name}.db`);
}

function makeEvent(data: string) {
  return {
    type: "file",
    category: "file",
    data,
    priority: 2,
    data_hash: "",
  };
}

function createLifetimeScannerDriver(opts?: { failClose?: boolean }) {
  let open = 0;
  let maxOpen = 0;
  let closes = 0;
  let finalized = 0;
  const closeArgs: boolean[] = [];
  const paths: string[] = [];

  class Driver {
    constructor(path: string) {
      open++;
      maxOpen = Math.max(maxOpen, open);
      paths.push(path);
      return new BunSQLiteAdapter({
        prepare(sql: string) {
          return {
            run: () => undefined,
            get: () => {
              if (sql.includes("COUNT(*)")) return { cnt: 1, bytes: 0 };
              return undefined;
            },
            all: () => [],
            iterate: function* () {},
            finalize: () => { finalized++; },
          };
        },
        transaction: (fn: unknown) => fn,
        close: (force: boolean) => {
          closes++;
          closeArgs.push(force);
          if (opts?.failClose) throw new Error("close failed");
          open--;
        },
      });
    }
  }

  return {
    loadDatabase: () => Driver,
    maxOpen: () => maxOpen,
    open: () => open,
    closes: () => closes,
    finalized: () => finalized,
    closeArgs: () => closeArgs,
    paths: () => paths,
  };
}

function createRetryingLifetimeScannerDriver(failuresBeforeClose: number) {
  let opens = 0;
  let liveHandles = 0;
  let closeAttempts = 0;

  class Driver {
    constructor(path: string) {
      opens++;
      liveHandles++;
      let failuresRemaining = failuresBeforeClose;
      return new BunSQLiteAdapter({
        prepare(sql: string) {
          return {
            run: () => undefined,
            get: () => sql.includes("COUNT(*)") ? { cnt: 1, bytes: 0 } : undefined,
            all: () => [],
            iterate: function* () {},
            finalize: () => undefined,
          };
        },
        transaction: (fn: unknown) => fn,
        close: () => {
          closeAttempts++;
          if (failuresRemaining-- > 0) throw new Error("close failed");
          liveHandles--;
        },
      });
    }
  }

  return {
    loadDatabase: () => Driver,
    opens: () => opens,
    liveHandles: () => liveHandles,
    closeAttempts: () => closeAttempts,
  };
}

function createRetryingConversationScannerDriver(failuresBeforeClose: number) {
  let opens = 0;
  let liveHandles = 0;
  let closeAttempts = 0;

  class Driver {
    constructor(_path: string) {
      opens++;
      liveHandles++;
      let failuresRemaining = failuresBeforeClose;
      return new BunSQLiteAdapter({
        prepare(sql: string) {
          return {
            run: () => undefined,
            get: () => {
              if (sql.includes("MIN(created_at)")) return { mn: null, mx: null };
              if (sql.includes("FROM session_resume")) return { bytes: 0, n: 0, lastSec: null };
              return undefined;
            },
            all: () => sql.includes("GROUP BY category") ? [{ category: "file", cnt: 1 }] : [],
            iterate: function* () {},
            finalize: () => undefined,
          };
        },
        transaction: (fn: unknown) => fn,
        close: () => {
          closeAttempts++;
          if (failuresRemaining-- > 0) throw new Error("close failed");
          liveHandles--;
        },
      });
    }
  }

  return {
    loadDatabase: () => Driver,
    opens: () => opens,
    liveHandles: () => liveHandles,
    closeAttempts: () => closeAttempts,
  };
}

function createRealBytesDriver(failClose: (path: string) => boolean) {
  const readonlyPaths: string[] = [];
  const closeAttempts: string[] = [];

  class Driver {
    constructor(path: string, opts?: { readonly?: boolean }) {
      if (!opts?.readonly) {
        return {
          pragma: () => [],
          exec: () => undefined,
          close: () => undefined,
        };
      }
      readonlyPaths.push(path);
      return {
        prepare(sql: string) {
          return {
            get: () => {
              if (sql.includes("FROM chunks")) return { bytes: 40 };
              if (sql.includes("FROM session_events")) {
                return { data_bytes: 10, bytes_avoided: 20, bytes_returned: 30 };
              }
              return { bytes: 0 };
            },
          };
        },
        close: () => {
          closeAttempts.push(path);
          if (failClose(path)) throw new Error("close failed");
        },
      };
    }
  }

  return {
    loadDatabase: () => Driver,
    readonlyPaths: () => readonlyPaths,
    closeAttempts: () => closeAttempts,
  };
}

function writeAdapterDb(home: string, adapter: string, name: string): { sessionsDir: string; contentDbPath: string } {
  const base = join(home, `.${adapter}`, "context-mode");
  const sessionsDir = join(base, "sessions");
  const contentDir = join(base, "content");
  mkdirSync(sessionsDir, { recursive: true });
  mkdirSync(contentDir, { recursive: true });
  writeFileSync(join(sessionsDir, `${name}.db`), "");
  const contentDbPath = join(contentDir, "content.db");
  writeFileSync(contentDbPath, "");
  return { sessionsDir, contentDbPath };
}

function withoutWarnings<T>(fn: () => T): T {
  const warning = console.warn;
  console.warn = () => undefined;
  try {
    return fn();
  } finally {
    console.warn = warning;
  }
}

describe("getLifetimeStats — cross-session totals + auto-memory", () => {
  test("retries one retained conversation handle before opening another", () => {
    const sessionsDir = tmpDir("conversation-retained-close");
    writeFileSync(join(sessionsDir, "one.db"), "");
    const driver = createRetryingConversationScannerDriver(Number.POSITIVE_INFINITY);

    withoutWarnings(() => {
      for (let i = 0; i < 5; i++) {
        expect(getConversationStats({ sessionId: "session", sessionsDir, loadDatabase: driver.loadDatabase }).events).toBe(1);
      }
    });

    expect(driver.opens()).toBe(1);
    expect(driver.liveHandles()).toBe(1);
    expect(driver.closeAttempts()).toBe(5);
  });

  test("resumes conversation scanning after a retained close succeeds", () => {
    const sessionsDir = tmpDir("conversation-retained-retry");
    writeFileSync(join(sessionsDir, "one.db"), "");
    const driver = createRetryingConversationScannerDriver(1);

    withoutWarnings(() => {
      expect(getConversationStats({ sessionId: "session", sessionsDir, loadDatabase: driver.loadDatabase }).events).toBe(1);
      expect(getConversationStats({ sessionId: "session", sessionsDir, loadDatabase: driver.loadDatabase }).events).toBe(1);
    });

    expect(driver.opens()).toBe(2);
    expect(driver.liveHandles()).toBe(1);
    expect(driver.closeAttempts()).toBe(3);
  });

  test("retries one retained session-sidecar handle without opening duplicates", () => {
    const sessionsDir = tmpDir("scanner-retained-close");
    writeFileSync(join(sessionsDir, "one.db"), "");
    const driver = createRetryingLifetimeScannerDriver(Number.POSITIVE_INFINITY);

    withoutWarnings(() => {
      for (let i = 0; i < 5; i++) {
        expect(getLifetimeStats({ sessionsDir, memoryRoot: tmpDir("scanner-memory"), loadDatabase: driver.loadDatabase }).totalEvents).toBe(1);
      }
    });

    expect(driver.opens()).toBe(1);
    expect(driver.liveHandles()).toBe(1);
    expect(driver.closeAttempts()).toBe(5);
  });

  test("resumes session-sidecar scanning after a retained close succeeds", () => {
    const sessionsDir = tmpDir("scanner-retained-retry");
    writeFileSync(join(sessionsDir, "one.db"), "");
    const driver = createRetryingLifetimeScannerDriver(1);

    withoutWarnings(() => {
      expect(getLifetimeStats({ sessionsDir, memoryRoot: tmpDir("scanner-memory"), loadDatabase: driver.loadDatabase }).totalEvents).toBe(1);
      expect(getLifetimeStats({ sessionsDir, memoryRoot: tmpDir("scanner-memory"), loadDatabase: driver.loadDatabase }).totalEvents).toBe(1);
    });

    expect(driver.opens()).toBe(2);
    expect(driver.liveHandles()).toBe(1);
    expect(driver.closeAttempts()).toBe(3);
  });

  test("preserves first content statistics and quarantines unresolved close handles", () => {
    const sessionContentDbPath = join(tmpDir("content-close-error-session"), "content.db");
    const allContentDbPath = join(tmpDir("content-close-error-all"), "content.db");
    writeFileSync(sessionContentDbPath, "");
    writeFileSync(allContentDbPath, "");
    const warnings: unknown[][] = [];
    const warning = console.warn;
    console.warn = (...args: unknown[]) => warnings.push(args);
    let opens = 0;
    const loadDatabase = () => class {
      constructor() { opens++; }
      prepare() {
        return { get: () => ({ bytes: 42 }) };
      }
      close() {
        throw new Error("close failed");
      }
    };
    try {
      for (let i = 0; i < 5; i++) {
        expect(getContentBytesForSession("session", sessionContentDbPath, { loadDatabase })).toBe(42);
        expect(getContentBytesAllSessions(allContentDbPath, { loadDatabase })).toBe(42);
      }
    } finally {
      console.warn = warning;
    }
    expect(opens).toBe(2);
    expect(warnings).toHaveLength(10);
    expect(warnings.every(([message]) => String(message).includes("close failed"))).toBe(true);
  });

  test("bounds repeated lifetime and multi-adapter scans to one open database", () => {
    const sessionsDir = tmpDir("scanner-lifetime");
    writeFileSync(join(sessionsDir, "one.db"), "");
    writeFileSync(join(sessionsDir, "two.db"), "");
    const home = tmpDir("scanner-home");
    const adapterDir = join(home, ".claude", "context-mode", "sessions");
    mkdirSync(adapterDir, { recursive: true });
    writeFileSync(join(adapterDir, "three.db"), "");
    const driver = createLifetimeScannerDriver();

    for (let i = 0; i < 20; i++) {
      getLifetimeStats({ sessionsDir, memoryRoot: tmpDir("scanner-memory"), loadDatabase: driver.loadDatabase });
    }
    getMultiAdapterLifetimeStats({ home, loadDatabase: driver.loadDatabase });

    expect(driver.maxOpen()).toBe(1);
    expect(driver.open()).toBe(0);
    expect(driver.closes()).toBe(41);
    expect(driver.finalized()).toBeGreaterThan(0);
    expect(driver.closeArgs()).toEqual(Array(41).fill(true));
  });

  test("stops scanner loops after path-qualified close failures without discarding readable stats", () => {
    const sessionsDir = tmpDir("scanner-close-error");
    writeFileSync(join(sessionsDir, "first.db"), "");
    writeFileSync(join(sessionsDir, "second.db"), "");
    const lifetimeDriver = createLifetimeScannerDriver({ failClose: true });
    const home = tmpDir("scanner-close-error-home");
    const adapterDir = join(home, ".claude", "context-mode", "sessions");
    mkdirSync(adapterDir, { recursive: true });
    writeFileSync(join(adapterDir, "first.db"), "");
    writeFileSync(join(adapterDir, "second.db"), "");
    const warning = console.warn;
    const warnings: unknown[][] = [];
    const adapterDriver = createLifetimeScannerDriver({ failClose: true });
    console.warn = (...args: unknown[]) => warnings.push(args);
    try {
      const stats = getLifetimeStats({ sessionsDir, memoryRoot: tmpDir("scanner-memory"), loadDatabase: lifetimeDriver.loadDatabase });
      expect(stats.totalEvents).toBe(1);
      expect(lifetimeDriver.paths()).toHaveLength(1);
      expect(lifetimeDriver.maxOpen()).toBe(1);
      expect(lifetimeDriver.open()).toBe(1);
      expect(getMultiAdapterLifetimeStats({ home, loadDatabase: adapterDriver.loadDatabase }).totalEvents).toBe(1);
      expect(adapterDriver.paths()).toHaveLength(1);
      expect(adapterDriver.maxOpen()).toBe(1);
      expect(adapterDriver.open()).toBe(1);
    } finally {
      console.warn = warning;
    }

    expect(warnings).toEqual([
      [expect.stringContaining(lifetimeDriver.paths()[0])],
      [expect.stringContaining(adapterDriver.paths()[0])],
    ]);
    expect(warnings.every(([message]) => String(message).includes("close failed"))).toBe(true);
  });

  test("stops a conversation composite after its first session close failure", () => {
    const sessionsDir = tmpDir("conversation-close-error");
    const first = join(sessionsDir, "worktree-first.db");
    const second = join(sessionsDir, "worktree-second.db");
    const contentDbPath = join(tmpDir("conversation-content-close-error"), "content.db");
    writeFileSync(first, "");
    writeFileSync(second, "");
    writeFileSync(contentDbPath, "");
    const driver = createRealBytesDriver(() => true);

    const stats = withoutWarnings(() => getConversationWindowStats({
      sessionId: "session",
      worktreeHash: "worktree",
      sessionsDir,
      contentDbPath,
      loadDatabase: driver.loadDatabase,
    } as any));

    expect(stats.eventDataBytes).toBe(10);
    expect(stats.bytesReturned).toBe(30);
    expect(driver.readonlyPaths()).toHaveLength(1);
    expect(driver.readonlyPaths()).not.toContain(contentDbPath);
  });

  test("stops multi-adapter lifetime scanning after a close failure", () => {
    const home = tmpDir("multi-adapter-lifetime-close-error");
    writeAdapterDb(home, "claude", "first");
    writeAdapterDb(home, "codex", "later");
    const driver = createLifetimeScannerDriver({ failClose: true });

    const stats = withoutWarnings(() => getMultiAdapterLifetimeStats({ home, loadDatabase: driver.loadDatabase }));

    expect(stats.totalEvents).toBe(1);
    expect(stats.perAdapter.map(({ name }) => name)).toEqual(["claude-code"]);
    expect(driver.paths()).toHaveLength(1);
  });

  test("stops multi-adapter real-byte scans before content or later adapters after a session close failure", () => {
    const home = tmpDir("multi-adapter-real-session-close-error");
    const claude = writeAdapterDb(home, "claude", "first");
    const codex = writeAdapterDb(home, "codex", "later");
    const driver = createRealBytesDriver(() => true);

    const stats = withoutWarnings(() => getMultiAdapterRealBytesStats({ home, loadDatabase: driver.loadDatabase }));

    expect(stats.eventDataBytes).toBe(10);
    expect(stats.perAdapter.map(({ name }) => name)).toEqual(["claude-code"]);
    expect(driver.readonlyPaths()).toEqual([join(claude.sessionsDir, "first.db")]);
    expect(driver.readonlyPaths()).not.toContain(claude.contentDbPath);
    expect(driver.readonlyPaths()).not.toContain(join(codex.sessionsDir, "later.db"));
  });

  test("stops multi-adapter real-byte scans after a content close failure", () => {
    const home = tmpDir("multi-adapter-real-content-close-error");
    const claude = writeAdapterDb(home, "claude", "first");
    const codex = writeAdapterDb(home, "codex", "later");
    const driver = createRealBytesDriver((path) => path === claude.contentDbPath);

    const stats = withoutWarnings(() => getMultiAdapterRealBytesStats({ home, loadDatabase: driver.loadDatabase }));

    expect(stats.eventDataBytes).toBe(10);
    expect(stats.contentBytes).toBe(40);
    expect(stats.perAdapter.map(({ name }) => name)).toEqual(["claude-code"]);
    expect(driver.readonlyPaths()).toContain(claude.contentDbPath);
    expect(driver.readonlyPaths()).not.toContain(join(codex.sessionsDir, "later.db"));
  });

  test("aggregates totalEvents and totalSessions across multiple SessionDBs", () => {
    const sessionsDir = tmpDir("sessions");

    const db1 = new SessionDB({ dbPath: tmpDbPath(sessionsDir, "proj-a") });
    cleanups.push(() => db1.cleanup());
    db1.ensureSession("sess-a1", "/p/a");
    db1.insertEvent("sess-a1", makeEvent("/p/a/x.ts"), "PostToolUse");
    db1.ensureSession("sess-a2", "/p/a");
    db1.insertEvent("sess-a2", makeEvent("/p/a/y.ts"), "PostToolUse");
    db1.close();

    const db2 = new SessionDB({ dbPath: tmpDbPath(sessionsDir, "proj-b") });
    cleanups.push(() => db2.cleanup());
    db2.ensureSession("sess-b1", "/p/b");
    db2.insertEvent("sess-b1", makeEvent("/p/b/m.ts"), "PostToolUse");
    db2.insertEvent("sess-b1", makeEvent("/p/b/n.ts"), "PostToolUse");
    db2.close();

    const memoryRoot = tmpDir("projects-empty");

    const stats = getLifetimeStats({ sessionsDir, memoryRoot });
    expect(stats.totalEvents).toBe(4);
    expect(stats.totalSessions).toBe(3);
  });

  test("counts auto-memory files across project subdirs", () => {
    const sessionsDir = tmpDir("sessions-empty");
    const memoryRoot = tmpDir("projects-with-memory");

    // ~/.claude/projects/<project>/memory/<file>.md
    const projA = join(memoryRoot, "proj-a", "memory");
    const projB = join(memoryRoot, "proj-b", "memory");
    mkdirSync(projA, { recursive: true });
    mkdirSync(projB, { recursive: true });

    writeFileSync(join(projA, "user_identity.md"), "name: Mert");
    writeFileSync(join(projA, "feedback_push.md"), "always push to next");
    writeFileSync(join(projB, "project_notes.md"), "hello");
    // Non-md file should be ignored
    writeFileSync(join(projB, "ignore.txt"), "skip me");

    const stats = getLifetimeStats({ sessionsDir, memoryRoot });
    expect(stats.autoMemoryCount).toBe(3);
    expect(stats.autoMemoryProjects).toBe(2);
  });

  test("returns zero stats when no DBs and no memory dirs exist", () => {
    const sessionsDir = tmpDir("none-sessions");
    const memoryRoot = tmpDir("none-memory");
    const stats = getLifetimeStats({ sessionsDir, memoryRoot });
    expect(stats.totalEvents).toBe(0);
    expect(stats.totalSessions).toBe(0);
    expect(stats.autoMemoryCount).toBe(0);
    expect(stats.autoMemoryProjects).toBe(0);
  });

  // ── Cycle 2: aggregate per-category counts across every SessionDB ──
  test("aggregates categoryCounts across multiple SessionDBs", () => {
    const sessionsDir = tmpDir("sessions-cats");

    function eventOfCategory(cat: string, data: string) {
      return { type: cat, category: cat, data, priority: 2, data_hash: "" };
    }

    const db1 = new SessionDB({ dbPath: tmpDbPath(sessionsDir, "proj-a") });
    cleanups.push(() => db1.cleanup());
    db1.ensureSession("sess-a1", "/p/a");
    db1.insertEvent("sess-a1", eventOfCategory("file", "/p/a/x.ts"), "PostToolUse");
    db1.insertEvent("sess-a1", eventOfCategory("file", "/p/a/y.ts"), "PostToolUse");
    db1.insertEvent("sess-a1", eventOfCategory("cwd", "/p/a"), "PostToolUse");
    db1.close();

    const db2 = new SessionDB({ dbPath: tmpDbPath(sessionsDir, "proj-b") });
    cleanups.push(() => db2.cleanup());
    db2.ensureSession("sess-b1", "/p/b");
    db2.insertEvent("sess-b1", eventOfCategory("file", "/p/b/m.ts"), "PostToolUse");
    db2.insertEvent("sess-b1", eventOfCategory("rule", "AGENTS.md"), "PostToolUse");
    db2.close();

    const memoryRoot = tmpDir("projects-empty-cats");
    const stats = getLifetimeStats({ sessionsDir, memoryRoot });
    expect(stats.categoryCounts).toBeDefined();
    expect(stats.categoryCounts.file).toBe(3); // 2 from a + 1 from b
    expect(stats.categoryCounts.cwd).toBe(1);
    expect(stats.categoryCounts.rule).toBe(1);
  });
});
