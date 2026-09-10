import { mkdirSync, mkdtempSync, rmSync, writeFileSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { afterAll, describe, expect, test } from "vitest";
import { QcNativeError, repoQcNative, runQcNative, verifyQcNative } from "../src/native-qc.js";

const nativeBin = process.env.QUIET_CONTEXT_NATIVE_TEST_BIN;
const suite = nativeBin ? describe : describe.skip;
const roots: string[] = [];
function shellCommand(script: string): string[] {
  return process.platform === "win32"
    ? ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive", "-Command", script]
    : ["sh", "-c", script];
}

function stopRepoDaemon(stateDir: string): void {
  try {
    const pid = Number(readFileSync(join(stateDir, "repomap", "repomap-v2.pid"), "utf8").trim());
    if (Number.isInteger(pid) && pid > 1) process.kill(pid, "SIGTERM");
  } catch { /* no daemon or already stopped */ }
}
afterAll(() => { for (const root of roots) { stopRepoDaemon(join(root, "state")); rmSync(root, { recursive: true, force: true }); } });

function envFor(root: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    QUIET_CONTEXT_NATIVE_BIN: nativeBin!,
    QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE: "1",
    QUIET_CONTEXT_NATIVE_STATE_DIR: join(root, "state"),
    QUIET_CONTEXT_NATIVE_SPOOL_DIR: join(root, "spool"),
    QUIET_CONTEXT_SESSION_ID: "integration",
  };
}

suite("qc-native real integration", () => {
  test("handshake, exact exit code and raw evidence", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-native-int-")); roots.push(root);
    const env = envFor(root);
    const status = await verifyQcNative({ env });
    expect(status.product).toBe("QuietContext");
    const receipt = await runQcNative(shellCommand(process.platform === "win32" ? "[Console]::Out.Write('out'); [Console]::Error.Write('err'); exit 7" : "printf out; printf err >&2; exit 7"), { env, cwd: root });
    expect(receipt.exitCode).toBe(7);
    expect(readFileSync(receipt.stdout.rawPath, "utf8")).toBe("out");
    expect(readFileSync(receipt.stderr.rawPath, "utf8")).toBe("err");
    expect(receipt.stdout.rawComplete).toBe(true);
    expect(receipt.stderr.rawComplete).toBe(true);
  });

  test("repo map/symbol/references/outline share one project root", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-native-repo-")); roots.push(root);
    mkdirSync(join(root, "src"));
    writeFileSync(join(root, "src", "app.ts"), "export function needle() { return 1; }\nexport const x = needle();\n");
    const env = envFor(root);
    const map = await repoQcNative({ action: "map", root }, { env, cwd: root });
    expect(map.exitCode).toBe(0);
    expect(map.stdout).toContain("needle");
    const symbol = await repoQcNative({ action: "symbol", query: "needle", root }, { env, cwd: root });
    expect(symbol.stdout).toContain("src/app.ts");
    const refs = await repoQcNative({ action: "references", query: "needle", root }, { env, cwd: root });
    expect(refs.stdout).toContain("needle()");
    const outline = await repoQcNative({ action: "outline", path: "src/app.ts", root }, { env, cwd: root });
    expect(outline.stdout).toContain("needle");
  });

  test("bridge timeout kills the native command tree", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-native-timeout-")); roots.push(root);
    const env = envFor(root);
    await expect(runQcNative(shellCommand(process.platform === "win32" ? "Start-Sleep -Seconds 5" : "sleep 5"), { env, cwd: root, timeoutMs: 50 }))
      .rejects.toEqual(expect.objectContaining<QcNativeError>({ code: "timeout" }));
  });
});
