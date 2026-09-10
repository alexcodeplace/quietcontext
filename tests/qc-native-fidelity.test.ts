import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { delimiter, join } from "node:path";
import { tmpdir } from "node:os";
import { afterAll, describe, expect, test } from "vitest";
import { runQcNative } from "../src/native-qc.js";

const nativeBin = process.env.QUIET_CONTEXT_NATIVE_TEST_BIN;
const suite = nativeBin ? describe : describe.skip;
const roots: string[] = [];
afterAll(() => { for (const root of roots) rmSync(root, { recursive: true, force: true }); });

function fixture(): { root: string; bin: string; env: NodeJS.ProcessEnv } {
  const root = mkdtempSync(join(tmpdir(), "qc-native-fidelity-")); roots.push(root);
  const bin = join(root, "bin"); mkdirSync(bin);
  const env = {
    ...process.env,
    PATH: `${bin}${delimiter}${process.env.PATH ?? ""}`,
    QUIET_CONTEXT_NATIVE_BIN: nativeBin!,
    QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE: "1",
    QUIET_CONTEXT_NATIVE_STATE_DIR: join(root, "state"),
    QUIET_CONTEXT_NATIVE_SPOOL_DIR: join(root, "spool"),
    QUIET_CONTEXT_NATIVE_RAW_MAX_BYTES: String(8 * 1024 * 1024),
    QUIET_CONTEXT_SESSION_ID: "fidelity",
  };
  return { root, bin, env };
}

function script(bin: string, name: string, body: string): void {
  const path = join(bin, name);
  writeFileSync(path, `#!/bin/sh\nset -eu\n${body}\n`);
  chmodSync(path, 0o755);
}

suite("qc-native command fidelity corpus", () => {
  test.skipIf(process.platform === "win32")("small output stays small and exact", async () => {
    const { root, bin, env } = fixture();
    script(bin, "tiny", "printf 'ok\\n'");
    const result = await runQcNative(["tiny"], { cwd: root, env });
    expect(result.exitCode).toBe(0);
    expect(result.stdout.compact).toBe("ok\n");
    expect(result.stdout.compactBytes).toBeLessThanOrEqual(result.stdout.rawBytes);
    expect(readFileSync(result.stdout.rawPath, "utf8")).toBe("ok\n");
  });

  test.skipIf(process.platform === "win32")("large successful and failing commands preserve exit code and exact raw evidence", async () => {
    const { root, bin, env } = fixture();
    script(bin, "cat", "i=1; while [ $i -le 180 ]; do if [ $i -eq 150 ]; then echo UNIQUE_MIDDLE_CANARY_42; else echo line-$i; fi; i=$((i+1)); done");
    const ok = await runQcNative(["cat"], { cwd: root, env });
    expect(ok.exitCode).toBe(0);
    expect(ok.stdout.compact).toContain("[showing first 100 of 180 lines]");
    expect(ok.stdout.compact).not.toContain("UNIQUE_MIDDLE_CANARY_42");
    expect(readFileSync(ok.stdout.rawPath, "utf8")).toContain("UNIQUE_MIDDLE_CANARY_42");

    script(bin, "pytest", "echo 'FAILED test_widget.py::test_canary - AssertionError: EXPECTED_CANARY'; exit 23");
    const failed = await runQcNative(["pytest"], { cwd: root, env });
    expect(failed.exitCode).toBe(23);
    expect(`${failed.stdout.compact}\n${failed.stderr.compact}`).toContain("FAILED");
  });

  test.skipIf(process.platform === "win32")("representative filter families never invent a success exit code", async () => {
    const { root, bin, env } = fixture();
    const cases = [
      ["cargo", "echo 'error[E0001]: BUILD_CANARY'; exit 17"],
      ["npm", "echo 'npm ERR! TEST_CANARY'; exit 19"],
      ["rg", "echo 'src/a.ts:12:RG_CANARY'; exit 2"],
      ["jq", "echo '{\"key\":\"JSON_CANARY\"}'; exit 5"],
      ["journalctl", "echo 'service: LOG_CANARY'; exit 9"],
    ] as const;
    for (const [name, body] of cases) {
      script(bin, name, body);
      const receipt = await runQcNative([name], { cwd: root, env });
      const expected = { cargo: 17, npm: 19, rg: 2, jq: 5, journalctl: 9 }[name];
      expect(receipt.exitCode, name).toBe(expected);
      expect(receipt.stdout.rawBytes, name).toBeGreaterThan(0);
    }
  });

  test.skipIf(process.platform === "win32")("ANSI and non-UTF8 output keep exact raw bytes", async () => {
    const { root, bin, env } = fixture();
    script(bin, "ansi-fixture", "printf '\\033[31mRED_CANARY\\033[0m\\n'");
    const ansi = await runQcNative(["ansi-fixture"], { cwd: root, env });
    expect(ansi.stdout.compact).toContain("RED_CANARY");
    expect(ansi.stdout.compact).not.toContain("\u001b[");
    expect(readFileSync(ansi.stdout.rawPath)).toContain(0x1b);

    script(bin, "binary-fixture", "printf '\\377\\376X'");
    const binary = await runQcNative(["binary-fixture"], { cwd: root, env });
    expect(binary.exitCode).toBe(0);
    expect([...readFileSync(binary.stdout.rawPath)]).toEqual([0xff, 0xfe, 0x58]);
  });

  test.runIf(process.platform === "win32")("native Windows execution preserves exact exit status and raw evidence", async () => {
    const { root, env } = fixture();
    const receipt = await runQcNative([
      "cmd.exe", "/d", "/s", "/c",
      "echo WINDOWS_NATIVE_CANARY& echo WINDOWS_NATIVE_ERR 1>&2& exit /b 23",
    ], { cwd: root, env });
    expect(receipt.exitCode).toBe(23);
    expect(readFileSync(receipt.stdout.rawPath, "utf8")).toContain("WINDOWS_NATIVE_CANARY");
    expect(readFileSync(receipt.stderr.rawPath, "utf8")).toContain("WINDOWS_NATIVE_ERR");
    expect(receipt.stdout.rawComplete).toBe(true);
    expect(receipt.stderr.rawComplete).toBe(true);
  });

  test("real git status/diff/log execute in the requested repository root", async () => {
    const { root, env } = fixture();
    const cp = await import("node:child_process");
    cp.execFileSync("git", ["init", "-q", "-b", "main"], { cwd: root });
    cp.execFileSync("git", ["config", "user.email", "qc@example.invalid"], { cwd: root });
    cp.execFileSync("git", ["config", "user.name", "QC Test"], { cwd: root });
    writeFileSync(join(root, "a.txt"), "one\n");
    cp.execFileSync("git", ["add", "a.txt"], { cwd: root });
    cp.execFileSync("git", ["commit", "-qm", "base"], { cwd: root });
    writeFileSync(join(root, "a.txt"), "one\ntwo\n");
    for (const argv of [["git", "status", "--short"], ["git", "diff"], ["git", "log", "-1", "--oneline"]]) {
      const receipt = await runQcNative(argv, { cwd: root, env });
      expect(receipt.exitCode, argv.join(" ")).toBe(0);
      expect(receipt.stdout.compactBytes, argv.join(" ")).toBeLessThanOrEqual(Math.max(receipt.stdout.rawBytes, 1) + 256);
    }
  });
});
