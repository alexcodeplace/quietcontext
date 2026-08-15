import { describe, expect, test } from "vitest";
import { readFileSync } from "node:fs";

describe("quietcontext hook surface", () => {
  test("does not register context injection or session-memory hooks", () => {
    const manifest = JSON.parse(readFileSync(".codex-plugin/hooks.json", "utf8"));
    expect(manifest.hooks ?? {}).toEqual({});
    const universal = JSON.parse(readFileSync("hooks/hooks.json", "utf8"));
    expect(universal.hooks ?? {}).toEqual({});
  });

  test("retains launcher self-healing", () => {
    const start = readFileSync("start.mjs", "utf8");
    expect(start).toContain("context-mode-cache-heal");
    expect(start).toContain("selfHealCacheHealHook");
  });

  test("search schema exposes indexed content only", () => {
    const schema = readFileSync("src/search/ctx-search-schema.ts", "utf8");
    expect(schema).not.toContain('enum(["relevance", "timeline"])');
    expect(schema).not.toContain("auto-memory");
  });

  test("does not auto-index session-memory event files", () => {
    const server = readFileSync("src/server.ts", "utf8");
    expect(server).not.toContain("maybeIndexSessionEvents");
  });

  test("does not perform update checks or emit update prompts", () => {
    const server = readFileSync("src/server.ts", "utf8");
    expect(server).not.toContain("fetchLatestVersion");
    expect(server).not.toContain("shouldShowVersionWarning");
  });

  test("registers quiet tools explicitly without inherited registration interception", () => {
    const server = readFileSync("src/server.ts", "utf8");
    expect(server).not.toContain("TOKEN_SAVING_TOOLS");
    expect(server).not.toContain("PUBLIC_TOOL_NAMES");
    expect(server).not.toMatch(/\.registerTool\s*=\s*/);
  });

  test("public docs and package metadata describe only QuietContext", () => {
    const readme = readFileSync("README.md", "utf8");
    const pkg = JSON.parse(readFileSync("package.json", "utf8"));
    expect(readme).toContain("Public names above are canonical");
    expect(readme).not.toContain("/plugin marketplace add");
    expect(pkg.repository).toBeUndefined();
    expect(pkg.homepage).toBeUndefined();
    expect(pkg.bugs).toBeUndefined();
  });

  test("platform instructions stay routing-only and compact", () => {
    const instructionFiles = [
      "CLAUDE.md",
      "configs/antigravity/GEMINI.md",
      "configs/claude-code/CLAUDE.md",
      "configs/codex/AGENTS.md",
      "configs/cursor/context-mode.mdc",
      "configs/gemini-cli/GEMINI.md",
      "configs/jetbrains-copilot/copilot-instructions.md",
      "configs/kilo/AGENTS.md",
      "configs/kiro/KIRO.md",
      "configs/omp/SYSTEM.md",
      "configs/openclaw/AGENTS.md",
      "configs/opencode/AGENTS.md",
      "configs/pi/AGENTS.md",
      "configs/qwen-code/QWEN.md",
      "configs/vscode-copilot/copilot-instructions.md",
      "configs/zed/AGENTS.md",
    ];

    for (const file of instructionFiles) {
      const instructions = readFileSync(file, "utf8");
      expect(Buffer.byteLength(instructions), file).toBeLessThanOrEqual(1_200);
      expect(instructions, file).toContain("quietcontext.execute");
      expect(instructions, file).not.toMatch(/ctx_(?:stats|doctor|upgrade|purge|insight)/);
      expect(instructions, file).not.toContain('sort: "timeline"');
      expect(instructions, file).not.toMatch(/^## (?:Memory|Session Continuity)$/m);
    }
  });
});
