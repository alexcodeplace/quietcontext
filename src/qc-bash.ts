import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export function qcBashRoutingMarkerPath(
  env: NodeJS.ProcessEnv = process.env,
  home: string = homedir(),
): string {
  const configRoot = env.XDG_CONFIG_HOME?.trim()
    ? env.XDG_CONFIG_HOME
    : join(home, ".config");
  return join(configRoot, "quietcontext", "qc-bash-routing.enabled");
}

/**
 * Activation precedence is explicit env override first, then durable marker.
 * `0` is an emergency kill switch even when the marker exists.
 */
export function qcBashRoutingEnabled(
  env: NodeJS.ProcessEnv = process.env,
  home: string = homedir(),
): boolean {
  if (env.QUIET_CONTEXT_QC_BASH_ROUTING === "0") return false;
  if (env.QUIET_CONTEXT_QC_BASH_ROUTING === "1") return true;
  try { return existsSync(qcBashRoutingMarkerPath(env, home)); }
  catch { return false; }
}

export interface QcBashDecision {
  rewrite: boolean;
  argv?: string[];
  rewrittenCommand?: string;
  reason: string;
}

const ALWAYS_WRAP = new Set([
  "rg", "grep", "egrep", "fgrep", "ack", "find", "tree",
  "cat", "bat", "ls", "exa", "eza", "diff", "colordiff",
  "cargo", "go", "tsc", "swc", "make", "cmake", "ninja",
  "jest", "vitest", "mocha", "playwright", "pytest", "phpunit",
  "npm", "pnpm", "yarn", "npx", "bun",
  "curl", "wget", "httpie", "http", "journalctl", "dmesg",
  "jq", "yq", "sqlite3", "psql", "mysql", "sed", "ps", "lsof",
]);
const GIT_READ_ONLY = new Set([
  "diff", "log", "show", "blame", "grep", "ls-files",
  "ls-tree", "diff-tree", "cat-file", "for-each-ref",
]);
const KUBECTL_READ_ONLY = new Set([
  "get", "describe", "logs", "events", "diff", "top", "api-resources",
  "api-versions", "version", "cluster-info", "explain",
]);
const CONTAINER_READ_ONLY = new Set(["logs", "ps", "inspect", "images", "version", "info"]);
const TERRAFORM_READ_ONLY = new Set(["show", "plan", "output", "validate", "version", "providers"]);
const SHELL_BUILTINS = new Set([
  "cd", "export", "unset", "alias", "unalias", "source", ".", "set", "shift",
  "read", "umask", "ulimit", "wait", "jobs", "fg", "bg", "exec", "eval", "trap",
]);
const PREFIX_WRAPPERS = new Set(["env", "sudo", "command", "timeout", "nice", "ionice", "chrt"]);

interface TokenizeResult { argv?: string[]; reason?: string }

/**
 * Parse only the shell subset whose semantics survive conversion from a Bash
 * source string to direct argv execution. Anything needing expansion,
 * redirection, pipelines, compound syntax or substitution is deliberately
 * rejected and left to the host Bash tool unchanged.
 */
export function tokenizeSimpleShell(command: string): TokenizeResult {
  if (!command.trim()) return { reason: "empty" };
  if (command.includes("\0") || command.includes("\n") || command.includes("\r")) {
    return { reason: "multiline-or-nul" };
  }
  const argv: string[] = [];
  let token = "";
  let tokenStarted = false;
  let state: "normal" | "single" | "double" = "normal";

  const push = () => {
    if (tokenStarted) argv.push(token);
    token = "";
    tokenStarted = false;
  };

  for (let i = 0; i < command.length; i++) {
    const ch = command[i];
    if (state === "single") {
      if (ch === "'") state = "normal";
      else token += ch;
      tokenStarted = true;
      continue;
    }
    if (state === "double") {
      if (ch === '"') {
        state = "normal";
        tokenStarted = true;
        continue;
      }
      if (ch === "$" || ch === "`") return { reason: "shell-expansion" };
      if (ch === "\\") {
        const next = command[++i];
        if (next === undefined) return { reason: "trailing-escape" };
        token += next;
        tokenStarted = true;
        continue;
      }
      token += ch;
      tokenStarted = true;
      continue;
    }

    if (/\s/.test(ch)) {
      push();
      continue;
    }
    if (ch === "'") { state = "single"; tokenStarted = true; continue; }
    if (ch === '"') { state = "double"; tokenStarted = true; continue; }
    if (ch === "\\") {
      const next = command[++i];
      if (next === undefined) return { reason: "trailing-escape" };
      token += next;
      tokenStarted = true;
      continue;
    }
    if ("|&;<>($`".includes(ch)) return { reason: "shell-operator" };
    if ("*?[]{}".includes(ch) || (ch === "~" && !tokenStarted)) return { reason: "shell-expansion" };
    if (ch === "#" && !tokenStarted) return { reason: "shell-comment" };
    token += ch;
    tokenStarted = true;
  }
  if (state !== "normal") return { reason: "unclosed-quote" };
  push();
  return argv.length ? { argv } : { reason: "empty" };
}

export function shellQuoteArg(value: string): string {
  if (/^[A-Za-z0-9_@%+=:,./-]+$/.test(value)) return value;
  return `'${value.replace(/'/g, `'"'"'`)}'`;
}

function commandIsWrappable(argv: string[]): { ok: boolean; reason: string } {
  if (argv.some((arg) => ["--version", "-V", "--help", "-h"].includes(arg))) {
    return { ok: false, reason: "bounded-probe" };
  }
  const command = argv[0];
  const base = command.includes("/") ? command.slice(command.lastIndexOf("/") + 1) : command;
  if (base === "qc" || base === "qc-native") return { ok: false, reason: "already-qc" };
  if (/^[A-Za-z_][A-Za-z0-9_]*=/.test(command)) return { ok: false, reason: "env-prefix" };
  if (SHELL_BUILTINS.has(base)) return { ok: false, reason: "shell-builtin" };
  if (PREFIX_WRAPPERS.has(base)) return { ok: false, reason: "wrapper-prefix" };
  if (ALWAYS_WRAP.has(base)) return { ok: true, reason: "supported-command" };
  if (base === "git") return { ok: GIT_READ_ONLY.has(argv[1] ?? ""), reason: "git-read-only" };
  if (base === "kubectl" || base === "k") return { ok: KUBECTL_READ_ONLY.has(argv[1] ?? ""), reason: "kubectl-read-only" };
  if (base === "docker" || base === "podman") {
    const interactive = argv.some((arg) => arg === "-t" || arg === "--tty" || /^-[^-]*t/.test(arg));
    return { ok: !interactive && CONTAINER_READ_ONLY.has(argv[1] ?? ""), reason: "container-read-only" };
  }
  if (base === "terraform" || base === "tofu") return { ok: TERRAFORM_READ_ONLY.has(argv[1] ?? ""), reason: "terraform-read-only" };
  return { ok: false, reason: "unsupported-command" };
}

export function classifyQcBashRewrite(command: string): QcBashDecision {
  const parsed = tokenizeSimpleShell(command);
  if (!parsed.argv) return { rewrite: false, reason: parsed.reason ?? "unsafe-shell-shape" };
  const decision = commandIsWrappable(parsed.argv);
  if (!decision.ok) return { rewrite: false, argv: parsed.argv, reason: decision.reason };
  return {
    rewrite: true,
    argv: parsed.argv,
    rewrittenCommand: `qc run -- ${parsed.argv.map(shellQuoteArg).join(" ")}`,
    reason: decision.reason,
  };
}
