import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { spawnSync } from "node:child_process";
import { afterAll, describe, expect, test } from "vitest";

const ROOT = resolve(import.meta.dirname, "..");
const QC = resolve(ROOT, "bin", "qc.mjs");
const roots: string[] = [];

function scratch(prefix: string): string {
  const root = mkdtempSync(join(tmpdir(), prefix));
  roots.push(root);
  return root;
}

afterAll(() => {
  for (const root of roots) rmSync(root, { recursive: true, force: true });
});

function baseEnv(root: string): NodeJS.ProcessEnv {
  return {
    ...process.env,
    QUIET_CONTEXT_DIR: join(root, "quiet-data"),
    QUIET_CONTEXT_PLATFORM: "claude-code",
  };
}

function runQc(args: string[], options: { cwd: string; env: NodeJS.ProcessEnv; input?: string | Buffer }) {
  return spawnSync(process.execPath, [QC, ...args], {
    cwd: options.cwd,
    env: options.env,
    input: options.input,
    encoding: "utf8",
    timeout: 30_000,
  });
}

describe("qc local content-store front door", () => {
  test("indexes bounded stdin and searches it through the same project store", () => {
    const root = scratch("qc-cli-index-");
    const project = join(root, "project");
    mkdirSync(project);
    const env = baseEnv(root);
    const canary = "QC_PROMPT_CANARY_91f0";
    const content = `${"prefix ".repeat(110)}\n${canary}\nsecond line kept in full output\n`;

    const indexed = runQc(
      ["index", "--stdin", "--source", "prompt/session-a/1", "--project", project],
      { cwd: project, env, input: content },
    );
    expect(indexed.status, indexed.stderr).toBe(0);
    expect(indexed.stdout).toContain("from stdin");
    expect(indexed.stdout).toContain("Source: prompt/session-a/1");

    const preview = runQc(
      ["search", canary, "--project", project, "--source", "prompt/session-a/", "--limit", "1"],
      { cwd: project, env },
    );
    expect(preview.status, preview.stderr).toBe(0);
    expect(preview.stdout).toContain("Source: prompt/session-a/1");
    expect(preview.stdout).not.toContain(canary);

    const full = runQc(
      ["search", canary, "--project", project, "--source", "prompt/session-a/", "--limit", "1", "--full"],
      { cwd: project, env },
    );
    expect(full.status, full.stderr).toBe(0);
    expect(full.stdout).toContain(canary);
    expect(full.stdout).toContain("second line kept in full output");
  });

  test("dispatches platform hooks through the qc front door without enabling routing", () => {
    const root = scratch("qc-cli-hook-");
    const project = join(root, "project");
    mkdirSync(project);
    const env = { ...baseEnv(root), QUIET_CONTEXT_QC_BASH_ROUTING: "0" };
    const input = JSON.stringify({
      session_id: "qc-cli-hook",
      tool_name: "Bash",
      tool_input: { command: "cargo test" },
      cwd: project,
    });

    const result = runQc(["hook", "codex", "pretooluse"], { cwd: project, env, input });
    expect(result.status, result.stderr).toBe(0);
    const output = JSON.parse(result.stdout.trim());
    expect(output.hookSpecificOutput?.hookEventName).toBe("PreToolUse");
    expect(output.hookSpecificOutput?.permissionDecision).not.toBe("deny");
  });

  test("accepts --label as a compatibility alias and rejects unsafe stdin", () => {
    const root = scratch("qc-cli-compat-");
    const project = join(root, "project");
    mkdirSync(project);
    const env = baseEnv(root);

    const labeled = runQc(
      ["index", "--stdin", "--label", "prompt/legacy/7", "--project", project],
      { cwd: project, env, input: "LEGACY_LABEL_CANARY_712c\n" },
    );
    expect(labeled.status, labeled.stderr).toBe(0);
    expect(labeled.stdout).toContain("Source: prompt/legacy/7");

    const missingSource = runQc(["index", "--stdin", "--project", project], {
      cwd: project,
      env,
      input: "no source\n",
    });
    expect(missingSource.status).toBe(1);
    expect(missingSource.stderr).toContain("--stdin requires --source");

    const invalidUtf8 = runQc(
      ["index", "--stdin", "--source", "invalid/utf8", "--project", project],
      { cwd: project, env, input: Buffer.from([0xff, 0xfe, 0xfd]) },
    );
    expect(invalidUtf8.status).toBe(1);
    expect(invalidUtf8.stderr).toContain("stdin must be valid UTF-8 text");
  });
});

const nativeBin = process.env.QUIET_CONTEXT_NATIVE_TEST_BIN;
const nativeSuite = nativeBin ? describe : describe.skip;

nativeSuite("qc repo CLI", () => {
  test("exposes map, symbol, references and outline through the packaged qc front door", () => {
    const root = scratch("qc-cli-repo-");
    const project = join(root, "project");
    mkdirSync(join(project, "src"), { recursive: true });
    writeFileSync(
      join(project, "src", "app.ts"),
      "export function CliNeedle() { return 1; }\nexport const x = CliNeedle();\n",
    );
    const env = {
      ...baseEnv(root),
      QUIET_CONTEXT_NATIVE_BIN: nativeBin!,
      QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE: "1",
      QUIET_CONTEXT_NATIVE_STATE_DIR: join(root, "native-state"),
      QUIET_CONTEXT_NATIVE_SPOOL_DIR: join(root, "spool"),
      QUIET_CONTEXT_SESSION_ID: "qc-cli-test",
    };

    const map = runQc(["repo", "map"], { cwd: project, env });
    expect(map.status, map.stderr).toBe(0);
    expect(map.stdout).toContain("CliNeedle");

    const symbol = runQc(["repo", "sym", "CliNeedle"], { cwd: project, env });
    expect(symbol.status, symbol.stderr).toBe(0);
    expect(symbol.stdout).toContain("src/app.ts");

    const refs = runQc(["repo", "refs", "CliNeedle"], { cwd: project, env });
    expect(refs.status, refs.stderr).toBe(0);
    expect(refs.stdout).toContain("CliNeedle()");

    const outline = runQc(["repo", "outline", "src/app.ts"], { cwd: project, env });
    expect(outline.status, outline.stderr).toBe(0);
    expect(outline.stdout).toContain("CliNeedle");

    // The native repo daemon must remain project scoped. Its PID is only a
    // cleanup hint here; the daemon itself is tested more deeply elsewhere.
    const pidFile = join(root, "native-state", "repomap", "repomap-v2.pid");
    try {
      const pid = Number(readFileSync(pidFile, "utf8").trim());
      if (Number.isInteger(pid) && pid > 1) process.kill(pid, "SIGTERM");
    } catch { /* already gone */ }
  });
});
