import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { describe, expect, test } from "vitest";
import { classifyQcBashRewrite, qcBashRoutingEnabled, qcBashRoutingMarkerPath, shellQuoteArg, tokenizeSimpleShell } from "../src/qc-bash.js";

describe("qc Bash rewrite classifier", () => {
  test("rewrites simple noisy commands with exact argv", () => {
    expect(classifyQcBashRewrite("rg -n 'hello world' src")).toMatchObject({
      rewrite: true,
      argv: ["rg", "-n", "hello world", "src"],
      rewrittenCommand: "qc run -- rg -n 'hello world' src",
    });
    expect(classifyQcBashRewrite("git status --short").rewrite).toBe(false);
    expect(classifyQcBashRewrite("cargo test -p app").rewrite).toBe(true);
  });

  test("leaves shell semantics to Bash instead of guessing", () => {
    const passThrough = [
      "FOO=bar cargo test",
      "rg foo | head",
      "rg foo > out.txt",
      "cargo test && echo done",
      "echo $(git status)",
      "cat <<EOF",
      "rg $PATTERN src",
      "rg *.ts src",
      "cd src",
      "source ./env.sh",
      "qc run -- cargo test",
      "git commit -m test",
      "kubectl apply -f x.yaml",
      "docker exec -it box sh",
    ];
    for (const command of passThrough) {
      expect(classifyQcBashRewrite(command).rewrite, command).toBe(false);
    }
  });

  test("quotes argv without reintroducing shell expansion", () => {
    expect(shellQuoteArg("plain/path")).toBe("plain/path");
    expect(shellQuoteArg("a b")).toBe("'a b'");
    expect(shellQuoteArg("it's")).toBe("'it'\"'\"'s'");
  });

  test("routing activation has a durable marker and an emergency env kill switch", () => {
    const root = mkdtempSync(join(tmpdir(), "qc-routing-marker-"));
    try {
      const env: NodeJS.ProcessEnv = { XDG_CONFIG_HOME: join(root, "config") };
      const marker = qcBashRoutingMarkerPath(env, root);
      expect(qcBashRoutingEnabled(env, root)).toBe(false);
      mkdirSync(join(root, "config", "quietcontext"), { recursive: true });
      writeFileSync(marker, "enabled\n");
      expect(qcBashRoutingEnabled(env, root)).toBe(true);
      expect(qcBashRoutingEnabled({ ...env, QUIET_CONTEXT_QC_BASH_ROUTING: "0" }, root)).toBe(false);
      expect(qcBashRoutingEnabled({ ...env, QUIET_CONTEXT_QC_BASH_ROUTING: "1" }, root)).toBe(true);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("parser preserves quoted literal operators but rejects active operators", () => {
    expect(tokenizeSimpleShell("rg 'a|b' src")).toEqual({ argv: ["rg", "a|b", "src"] });
    expect(tokenizeSimpleShell("rg a\\ b src")).toEqual({ argv: ["rg", "a b", "src"] });
    expect(tokenizeSimpleShell("rg a|b src")).toEqual({ reason: "shell-operator" });
  });
});
