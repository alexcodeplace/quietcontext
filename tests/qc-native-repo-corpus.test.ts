import { mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { afterAll, describe, expect, test } from "vitest";
import { repoQcNative } from "../src/native-qc.js";

const nativeBin = process.env.QUIET_CONTEXT_NATIVE_TEST_BIN;
const suite = nativeBin ? describe : describe.skip;
const roots: string[] = [];
function stopRepoDaemon(stateDir: string): void {
  try {
    const pid = Number(readFileSync(join(stateDir, "repomap", "repomap-v2.pid"), "utf8").trim());
    if (Number.isInteger(pid) && pid > 1) process.kill(pid, "SIGTERM");
  } catch { /* no daemon or already stopped */ }
}
afterAll(() => { for (const root of roots) { stopRepoDaemon(join(root, ".qc-state")); rmSync(root, { recursive: true, force: true }); } });

function makeRepo() {
  const root = mkdtempSync(join(tmpdir(), "qc-native-repo-corpus-")); roots.push(root);
  const state = join(root, ".qc-state");
  mkdirSync(join(root, "src"), { recursive: true });
  mkdirSync(join(root, ".git"), { recursive: true });
  writeFileSync(join(root, ".git", "HEAD"), "ref: refs/heads/main\n");
  mkdirSync(join(root, "node_modules", "ignored"), { recursive: true });
  writeFileSync(join(root, ".gitignore"), "ignored.py\n");
  writeFileSync(join(root, "src", "app.ts"), "export function tsNeedle() { return 1; }\nexport const tsUse = tsNeedle();\n");
  writeFileSync(join(root, "src", "worker.js"), "export function jsNeedle() { return 2; }\n");
  writeFileSync(join(root, "src", "lib.rs"), "pub fn rust_needle() -> i32 { 3 }\n");
  writeFileSync(join(root, "src", "worker.py"), "def py_needle():\n    return 4\n");
  writeFileSync(join(root, "ignored.py"), "def SECRET_IGNORED():\n    pass\n");
  writeFileSync(join(root, "node_modules", "ignored", "x.ts"), "export function NODE_MODULE_SECRET() {}\n");
  const env = {
    ...process.env,
    QUIET_CONTEXT_NATIVE_BIN: nativeBin!,
    QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE: "1",
    QUIET_CONTEXT_NATIVE_STATE_DIR: state,
    QUIET_CONTEXT_SESSION_ID: "repo-corpus",
  };
  return { root, state, env };
}

async function waitFor(predicate: () => Promise<boolean>, timeoutMs = 2000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (await predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  throw new Error("condition did not converge before deadline");
}

suite("qc-native repository navigation corpus", () => {
  test("map/symbol/references/outline cover TS/JS/Rust/Python and respect ignores", async () => {
    const { root, env } = makeRepo();
    const map = await repoQcNative({ action: "map", root }, { cwd: root, env });
    expect(map.exitCode).toBe(0);
    for (const needle of ["tsNeedle", "jsNeedle", "rust_needle", "py_needle"]) expect(map.stdout).toContain(needle);
    expect(map.stdout).not.toContain("SECRET_IGNORED");
    expect(map.stdout).not.toContain("NODE_MODULE_SECRET");

    const symbol = await repoQcNative({ action: "symbol", query: "tsNeedle", root }, { cwd: root, env });
    expect(symbol.stdout).toContain("src/app.ts");
    const refs = await repoQcNative({ action: "references", query: "tsNeedle", root }, { cwd: root, env });
    expect(refs.stdout).toContain("tsNeedle()");
    const outline = await repoQcNative({ action: "outline", path: "src/lib.rs", root }, { cwd: root, env });
    expect(outline.stdout).toContain("rust_needle");
  });

  test("cold cache becomes hit, then mutation/rename/delete reconcile to a newer generation", async () => {
    const { root, env } = makeRepo();
    const cold = await repoQcNative({ action: "map", root }, { cwd: root, env });
    expect(cold.exitCode).toBe(0);
    const warm = await repoQcNative({ action: "map", root }, { cwd: root, env });
    if (process.platform === "win32") {
      expect(warm.cacheState).toBe("bypassed");
      expect(warm.fallbackReason).toBe("unsupported-platform");
    } else {
      expect(["hit", "refreshed"]).toContain(warm.cacheState);
      expect(warm.generation).toBeGreaterThanOrEqual(cold.generation);
    }

    const app = join(root, "src", "app.ts");
    writeFileSync(app, "export function mutatedNeedle() { return 5; }\nexport const x = mutatedNeedle();\n");
    let mutatedGeneration = warm.generation;
    await waitFor(async () => {
      const symbol = await repoQcNative({ action: "symbol", query: "mutatedNeedle", root }, { cwd: root, env });
      mutatedGeneration = symbol.generation;
      return symbol.exitCode === 0 && symbol.stdout.includes("src/app.ts");
    });
    if (process.platform !== "win32") expect(mutatedGeneration).toBeGreaterThanOrEqual(warm.generation);

    const renamed = join(root, "src", "renamed.ts");
    renameSync(app, renamed);
    await waitFor(async () => {
      const map = await repoQcNative({ action: "map", root }, { cwd: root, env });
      return map.stdout.includes("src/renamed.ts") && !map.stdout.includes("src/app.ts");
    });
    unlinkSync(renamed);
    await waitFor(async () => {
      const map = await repoQcNative({ action: "map", root }, { cwd: root, env });
      return !map.stdout.includes("renamed.ts");
    });
  });

  test("daemon restart preserves semantic results and cold-start recovery", async () => {
    const { root, state, env } = makeRepo();
    const before = await repoQcNative({ action: "map", root }, { cwd: root, env });
    expect(before.stdout).toContain("tsNeedle");
    if (process.platform !== "win32") {
      const repomapState = join(state, "repomap");
      const pidFile = readFileSync(join(repomapState, "repomap-v2.pid"), "utf8").trim();
      const pid = Number(pidFile);
      expect(Number.isInteger(pid) && pid > 1).toBe(true);
      try { process.kill(pid, "SIGTERM"); } catch { /* already stopped */ }
      await new Promise((resolve) => setTimeout(resolve, 120));
    } else {
      expect(before.cacheState).toBe("bypassed");
      expect(before.fallbackReason).toBe("unsupported-platform");
    }
    const after = await repoQcNative({ action: "map", root }, { cwd: root, env });
    expect(after.exitCode).toBe(0);
    expect(after.stdout).toContain("tsNeedle");
    expect(after.stdout.replace(/generation[^\n]*/g, "")).toContain("src/app.ts");
  });

  test("explicit daemon-mode marker forces direct fallback without changing semantics", async () => {
    const { root, env } = makeRepo();
    const indexed = await repoQcNative({ action: "symbol", query: "py_needle", root }, { cwd: root, env });
    const blockedState = join(root, "blocked-state");
    writeFileSync(blockedState, "not-a-directory");
    const direct = await repoQcNative({ action: "symbol", query: "py_needle", root }, {
      cwd: root,
      env: { ...env, QUIET_CONTEXT_NATIVE_STATE_DIR: blockedState },
    });
    expect(indexed.exitCode).toBe(0);
    expect(direct.exitCode).toBe(0);
    expect(direct.cacheState).toBe("bypassed");
    if (process.platform !== "win32") expect(direct.fallbackReason).toBeTruthy();
    expect(direct.stdout).toBe(indexed.stdout);
  });
});
