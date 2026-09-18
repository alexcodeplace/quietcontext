import { afterEach, describe, expect, test } from "vitest";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const mod = await import(pathToFileURL(join(process.cwd(), "hooks", "qc-frontload.mjs")).href);
const { shouldFrontloadPrompt, resolveSafeFrontloadRoot, frontloadPromptContext } = mod;

const oldEnv = { ...process.env };
afterEach(() => {
  process.env = { ...oldEnv };
});

describe("QC prompt front-load", () => {
  test("classifies structural and code-shaped prompts without English-only dependence", () => {
    expect(shouldFrontloadPrompt("How does AuthService.login work?")).toBe(true);
    expect(shouldFrontloadPrompt("איך עובד AuthService.login?")).toBe(true);
    expect(shouldFrontloadPrompt("איך עובדת הארכיטקטורה של ההתחברות?")).toBe(true);
    expect(shouldFrontloadPrompt("comment marche la state machine des commandes ?")).toBe(true);
    expect(shouldFrontloadPrompt("why does login fail?")).toBe(true);
    expect(shouldFrontloadPrompt("prefix the label with a star")).toBe(false);
    expect(shouldFrontloadPrompt("thanks, got it")).toBe(false);
    expect(shouldFrontloadPrompt("/help")).toBe(false);
  });

  test("rejects home/root and finds only an ancestor project root", () => {
    expect(resolveSafeFrontloadRoot(homedir())).toBeNull();
    const root = mkdtempSync(join(tmpdir(), "qc-frontload-root-"));
    try {
      writeFileSync(join(root, "package.json"), "{}");
      const child = join(root, "src");
      mkdirSync(child);
      expect(resolveSafeFrontloadRoot(child)).toBe(root);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("injects bounded context from one daemon explore request", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-frontload-run-"));
    try {
      writeFileSync(join(root, "package.json"), "{}");
      process.env.QUIET_CONTEXT_ADOPTION_TELEMETRY_DISABLE = "1";
      let calls = 0;
      const ctx = await frontloadPromptContext("How does login work?", root, {
        timeoutMs: 500,
        requestExplore: async () => {
          calls += 1;
          return "[qc-explore v1] login\n## login - src/auth.ts:10\n[source]\n10\tfunction login() {}\n";
        },
      });
      expect(calls).toBe(1);
      expect(ctx).toContain("<qc_context");
      expect(ctx).toContain("[qc-explore v1]");
      expect(Buffer.byteLength(ctx)).toBeLessThanOrEqual(4 * 1024);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("non-structural prompts do zero explore work", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-frontload-noop-"));
    try {
      writeFileSync(join(root, "package.json"), "{}");
      let calls = 0;
      const ctx = await frontloadPromptContext("thanks, got it", root, {
        requestExplore: async () => {
          calls += 1;
          return "unexpected";
        },
      });
      expect(ctx).toBe("");
      expect(calls).toBe(0);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("no-match and timeout fail open with no injected context", async () => {
    const root = mkdtempSync(join(tmpdir(), "qc-frontload-fail-"));
    try {
      writeFileSync(join(root, "package.json"), "{}");
      process.env.QUIET_CONTEXT_ADOPTION_TELEMETRY_DISABLE = "1";
      const noMatch = await frontloadPromptContext("trace LoginService", root, {
        requestExplore: async () => "[qc-explore v1] x\nNo high-confidence symbol/file match.\n",
      });
      expect(noMatch).toBe("");

      const timeout = await frontloadPromptContext("trace LoginService", root, {
        requestExplore: async () => {
          const error = new Error("timed out");
          error.name = "AbortError";
          throw error;
        },
      });
      expect(timeout).toBe("");
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });
});
