import { afterEach, describe, expect, it } from "vitest";
import { existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ContentStore } from "../../src/store.js";
import { loadDatabase } from "../../src/db-base.js";
import {
  acquireContentStoreMigrationFence,
  acquireContentStoreOwnerFence,
} from "../../src/content-store-fence.js";
import {
  hashProjectDirCanonical,
  hashProjectDirLegacy,
  resolveContentStorePath,
} from "../../src/session/db.js";

const cleanup: string[] = [];
afterEach(() => {
  while (cleanup.length) {
    const p = cleanup.pop();
    if (p && existsSync(p)) rmSync(p, { recursive: true, force: true });
  }
});

function makeContentDir(): string {
  const d = mkdtempSync(join(tmpdir(), "ctx-content-"));
  cleanup.push(d);
  return d;
}

describe("content-store filesystem fences", () => {
  it("treats an incomplete migration marker as active", () => {
    const contentDir = makeContentDir();
    const dbPath = join(contentDir, "incomplete.db");
    const accessDir = `${dbPath}.access`;
    mkdirSync(accessDir, { recursive: true });
    const marker = join(accessDir, "migration");
    writeFileSync(marker, "");

    expect(() => acquireContentStoreOwnerFence(dbPath)).toThrow(/migration is active/i);
    expect(existsSync(marker)).toBe(true);
  });

  it("uses process start identity to clear a reused-PID marker", () => {
    const contentDir = makeContentDir();
    const dbPath = join(contentDir, "reused-pid.db");
    const accessDir = `${dbPath}.access`;
    mkdirSync(accessDir, { recursive: true });
    writeFileSync(
      join(accessDir, "migration"),
      JSON.stringify({ pid: process.pid, startToken: "not-this-process" }),
    );

    const release = acquireContentStoreOwnerFence(dbPath);
    release();
    expect(existsSync(join(accessDir, "migration"))).toBe(false);
  });

  it("publishes an active migration marker before admitting owners", () => {
    const contentDir = makeContentDir();
    const dbPath = join(contentDir, "active.db");
    const release = acquireContentStoreMigrationFence(dbPath);
    try {
      expect(() => acquireContentStoreOwnerFence(dbPath)).toThrow(/migration is active/i);
    } finally {
      release();
    }
  });
});

describe("resolveContentStorePath", () => {
  it("fresh install — no legacy, no canonical: returns canonical path (file not yet created)", () => {
    const projectDir = "/tmp/Some/Project";
    const contentDir = makeContentDir();
    const path = resolveContentStorePath({ projectDir, contentDir });
    expect(path.startsWith(contentDir)).toBe(true);
    expect(path.endsWith(".db")).toBe(true);
    expect(existsSync(path)).toBe(false); // caller opens it via ContentStore
  });

  it.skipIf(process.platform === "linux")(
    "Mac/Win: casing variants of same projectDir resolve to identical content store path",
    () => {
      const contentDir = makeContentDir();
      const upper = resolveContentStorePath({ projectDir: "/Users/Mert/X", contentDir });
      const lower = resolveContentStorePath({ projectDir: "/users/mert/x", contentDir });
      expect(upper).toBe(lower);
    },
  );

  it.skipIf(process.platform === "linux")(
    "migrates a legacy raw-casing FTS5 database without renaming live files",
    () => {
      const projectDir = "/Users/Mert/MigrateMe";
      const contentDir = makeContentDir();
      const legacyHash = hashProjectDirLegacy(projectDir);
      const canonicalHash = hashProjectDirCanonical(projectDir);
      if (legacyHash === canonicalHash) return; // nothing to migrate

      const legacyMain = join(contentDir, `${legacyHash}.db`);
      const legacy = new ContentStore(legacyMain);
      legacy.index({ content: "legacy casing payload", source: "legacy-casing" });
      legacy.close();

      const resolved = resolveContentStorePath({ projectDir, contentDir });
      const canonicalPath = join(contentDir, `${canonicalHash}.db`);
      expect(resolved).toBe(canonicalPath);
      const migrated = new ContentStore(canonicalPath);
      expect(migrated.search("legacy casing")[0]?.source).toBe("legacy-casing");
      migrated.close();
      expect(existsSync(legacyMain)).toBe(true);
    },
  );

  it.skipIf(process.platform === "linux")(
    "DOES NOT migrate when both legacy AND canonical exist (data-loss safety)",
    () => {
      const projectDir = "/Users/Mert/BothExist";
      const contentDir = makeContentDir();
      const legacyHash = hashProjectDirLegacy(projectDir);
      const canonicalHash = hashProjectDirCanonical(projectDir);
      if (legacyHash === canonicalHash) return;

      const legacyPath = join(contentDir, `${legacyHash}.db`);
      const canonicalPath = join(contentDir, `${canonicalHash}.db`);
      writeFileSync(legacyPath, "LEGACY");
      writeFileSync(canonicalPath, "CANONICAL");

      expect(() => resolveContentStorePath({ projectDir, contentDir }))
        .toThrow(/Cannot safely converge/);
      expect(readFileSync(canonicalPath, "utf8")).toBe("CANONICAL");
      expect(readFileSync(legacyPath, "utf8")).toBe("LEGACY");
    },
  );

  it("main and linked worktrees resolve to the same content store", () => {
    const repoDir = mkdtempSync(join(tmpdir(), "ctx-content-repo-"));
    const linkedDir = `${repoDir}-linked`;
    const contentDir = makeContentDir();
    cleanup.push(repoDir, linkedDir);
    execFileSync("git", ["init", "-q", repoDir]);
    execFileSync("git", ["-C", repoDir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "init"]);
    execFileSync("git", ["-C", repoDir, "worktree", "add", "-q", "-b", "linked", linkedDir]);

    expect(resolveContentStorePath({ projectDir: repoDir, contentDir }))
      .toBe(resolveContentStorePath({ projectDir: linkedDir, contentDir }));
  });

  it("migrates a pre-upgrade linked-worktree database to the canonical store", () => {
    const repoDir = mkdtempSync(join(tmpdir(), "ctx-content-legacy-repo-"));
    const linkedDir = `${repoDir}-linked`;
    const contentDir = makeContentDir();
    cleanup.push(repoDir, linkedDir);
    execFileSync("git", ["init", "-q", repoDir]);
    execFileSync("git", ["-C", repoDir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "init"]);
    execFileSync("git", ["-C", repoDir, "worktree", "add", "-q", "-b", "legacy-linked", linkedDir]);
    const oldLinkedPath = join(contentDir, `${hashProjectDirCanonical(linkedDir)}.db`);
    const legacy = new ContentStore(oldLinkedPath);
    legacy.index({ content: "migrated linked payload", source: "legacy-linked" });
    legacy.close();

    const resolved = resolveContentStorePath({ projectDir: linkedDir, contentDir });
    const canonical = resolveContentStorePath({ projectDir: repoDir, contentDir });
    expect(resolved).toBe(canonical);
    expect(resolved).not.toBe(oldLinkedPath);
    const migrated = new ContentStore(resolved);
    expect(migrated.search("migrated")[0]?.source).toBe("legacy-linked");
    migrated.close();
  });

  it("fails closed for a pre-ownership legacy store", () => {
    const repoDir = mkdtempSync(join(tmpdir(), "ctx-content-ownerless-repo-"));
    const linkedDir = `${repoDir}-linked`;
    const contentDir = makeContentDir();
    cleanup.push(repoDir, linkedDir);
    execFileSync("git", ["init", "-q", repoDir]);
    execFileSync("git", ["-C", repoDir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "init"]);
    execFileSync("git", ["-C", repoDir, "worktree", "add", "-q", "-b", "ownerless-linked", linkedDir]);
    const legacyPath = join(contentDir, `${hashProjectDirCanonical(linkedDir)}.db`);
    const Database = loadDatabase();
    const legacy = new Database(legacyPath);
    legacy.exec("CREATE TABLE legacy_content (payload TEXT); INSERT INTO legacy_content VALUES ('ownerless')");
    legacy.close();

    expect(() => resolveContentStorePath({ projectDir: linkedDir, contentDir }))
      .toThrow(/Cannot safely migrate/);
    expect(existsSync(legacyPath)).toBe(true);
    expect(existsSync(resolveContentStorePath({ projectDir: repoDir, contentDir }))).toBe(false);
  });

  it("fails closed when a legacy store has an active peer", () => {
    const repoDir = mkdtempSync(join(tmpdir(), "ctx-content-live-repo-"));
    const linkedDir = `${repoDir}-linked`;
    const contentDir = makeContentDir();
    cleanup.push(repoDir, linkedDir);
    execFileSync("git", ["init", "-q", repoDir]);
    execFileSync("git", ["-C", repoDir, "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-qm", "init"]);
    execFileSync("git", ["-C", repoDir, "worktree", "add", "-q", "-b", "live-linked", linkedDir]);
    const legacyPath = join(contentDir, `${hashProjectDirCanonical(linkedDir)}.db`);
    const seed = new ContentStore(legacyPath);
    seed.index({ content: "live migration payload", source: "live-source" });
    seed.close();
    const peer = new ContentStore(legacyPath);
    const startedAt = Date.now();
    expect(() => resolveContentStorePath({ projectDir: linkedDir, contentDir })).toThrow(/Cannot safely migrate/);
    expect(Date.now() - startedAt).toBeLessThan(750);
    peer.close();
    expect(existsSync(resolveContentStorePath({ projectDir: linkedDir, contentDir }))).toBe(true);
  });

  it("different projects stay in separate FTS5 files (no cross-migration)", () => {
    const contentDir = makeContentDir();
    const p1 = resolveContentStorePath({ projectDir: "/x/proj-1", contentDir });
    const p2 = resolveContentStorePath({ projectDir: "/x/proj-2", contentDir });
    expect(p1).not.toBe(p2);
  });

  it.skipIf(process.platform !== "linux")(
    "Linux: legacyHash === canonicalHash so migration never attempts",
    () => {
      const projectDir = "/Users/Mert/LinuxNoOp";
      const contentDir = makeContentDir();
      expect(hashProjectDirLegacy(projectDir)).toBe(hashProjectDirCanonical(projectDir));
      const hash = hashProjectDirCanonical(projectDir);
      const filePath = join(contentDir, `${hash}.db`);
      writeFileSync(filePath, "LINUX");
      const resolved = resolveContentStorePath({ projectDir, contentDir });
      expect(resolved).toBe(filePath);
      expect(readFileSync(filePath, "utf8")).toBe("LINUX");
    },
  );
});
