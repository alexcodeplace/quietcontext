import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { delimiter, join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { afterEach, describe, expect, test } from "vitest";

const ROOT = resolve(import.meta.dirname, "..");
const scratch: string[] = [];
afterEach(() => { for (const dir of scratch.splice(0)) rmSync(dir, { recursive: true, force: true }); });

function fixture(codexVersion?: string) {
  const root = mkdtempSync(join(tmpdir(), "qc-bash-hook-")); scratch.push(root);
  const home = join(root, "home"); const bin = join(root, "bin"); const tmp = join(root, "tmp");
  mkdirSync(home, { recursive: true }); mkdirSync(bin); mkdirSync(tmp);
  if (codexVersion) {
    const codex = join(bin, process.platform === "win32" ? "codex.cmd" : "codex");
    writeFileSync(
      codex,
      process.platform === "win32"
        ? `@echo off\r\necho codex-cli ${codexVersion}\r\n`
        : `#!/bin/sh\nprintf 'codex-cli ${codexVersion}\\n'\n`,
    );
    if (process.platform !== "win32") chmodSync(codex, 0o755);
  }
  return {
    root,
    env: {
      ...process.env,
      HOME: home,
      CLAUDE_CONFIG_DIR: join(home, ".claude"),
      TMPDIR: tmp,
      PATH: `${bin}${delimiter}${process.env.PATH ?? ""}`,
      QUIET_CONTEXT_QC_BASH_ROUTING: "1",
    },
  };
}

function runHook(rel: string, input: Record<string, unknown>, env: NodeJS.ProcessEnv): any | null {
  const result = spawnSync(process.execPath, [join(ROOT, rel)], {
    cwd: ROOT,
    env,
    input: JSON.stringify(input),
    encoding: "utf8",
    timeout: 10_000,
  });
  expect(result.status, `${rel}\nstderr=${result.stderr}\nstdout=${result.stdout}`).toBe(0);
  const line = result.stdout.trim().split("\n").filter(Boolean).at(-1);
  return line ? JSON.parse(line) : null;
}

function bashInput(command: string, session = "qc-hook-test") {
  return {
    session_id: session,
    tool_name: "Bash",
    tool_input: { command },
    cwd: ROOT,
  };
}

describe("qc Bash routing hook", () => {
  test("Claude current-host path denies a noisy Bash call and gives exact qc retry", () => {
    const { env } = fixture();
    const out = runHook("hooks/pretooluse.mjs", bashInput("cargo test"), env);
    expect(out?.hookSpecificOutput?.permissionDecision).toBe("deny");
    expect(out?.hookSpecificOutput?.permissionDecisionReason).toContain("qc run -- cargo test");
  });

  test("Claude routing is an explicit cutover switch and keeps bounded/unsafe shell shapes alone", () => {
    const { env } = fixture();
    const disabled = runHook(
      "hooks/pretooluse.mjs",
      bashInput("cargo test", "qc-hook-disabled"),
      { ...env, QUIET_CONTEXT_QC_BASH_ROUTING: "0" },
    );
    expect(disabled?.hookSpecificOutput?.permissionDecision).not.toBe("deny");

    for (const [i, command] of ["git status --short", "cargo --version", "rg needle | head", "qc run -- cargo test"].entries()) {
      const out = runHook("hooks/pretooluse.mjs", bashInput(command, `qc-hook-safe-${i}`), env);
      expect(out?.hookSpecificOutput?.permissionDecision, command).not.toBe("deny");
    }
  });

  test("modern Codex transparently rewrites supported Bash to qc", () => {
    const { env } = fixture("0.141.0");
    const out = runHook("hooks/codex/pretooluse.mjs", bashInput("npm test", "qc-hook-codex-modern"), env);
    expect(out?.hookSpecificOutput?.permissionDecision).toBe("allow");
    expect(out?.hookSpecificOutput?.updatedInput?.command).toBe("qc run -- npm test");
  });

  test("older Codex fails closed with a qc retry instead of silently running raw Bash", () => {
    const { env } = fixture("0.140.0");
    const out = runHook("hooks/codex/pretooluse.mjs", bashInput("npm test", "qc-hook-codex-old"), env);
    expect(out?.hookSpecificOutput?.permissionDecision).toBe("deny");
    expect(out?.hookSpecificOutput?.permissionDecisionReason).toContain("qc run -- npm test");
  });
});
