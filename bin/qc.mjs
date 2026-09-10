#!/usr/bin/env node
import { chmodSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { repoQcNative, runQcNative, verifyQcNative, QcNativeError } from "../build/native-qc.js";
import { qcBashRoutingEnabled, qcBashRoutingMarkerPath } from "../build/qc-bash.js";

const pkgRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = JSON.parse(readFileSync(resolve(pkgRoot, "package.json"), "utf8"));
const args = process.argv.slice(2);

function help() {
  process.stdout.write([
    "QuietContext local runtime",
    "",
    "Usage:",
    "  qc run -- <command> [args...]",
    "  qc repo map [--root <path>]",
    "  qc repo symbol <name> [--root <path>]",
    "  qc repo references <name> [--root <path>]",
    "  qc repo outline <file> [--root <path>]",
    "  qc index <path> | --stdin --source <label> [--project <path>]",
    "  qc search <query...> [--project <path>] [--source <label>] [--full]",
    "  qc doctor",
    "  qc status",
    "  qc routing status|enable|disable",
    "  qc --version",
    "",
  ].join("\n"));
}


function parseRootOption(argv) {
  const rest = [];
  let root;
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--root") {
      root = argv[++i];
      if (!root) throw new Error("--root requires a path");
    } else if (arg.startsWith("--root=")) {
      root = arg.slice("--root=".length);
      if (!root) throw new Error("--root requires a path");
    } else {
      rest.push(arg);
    }
  }
  return { root, rest };
}

function runContextModeCli(argv) {
  const result = spawnSync(process.execPath, [resolve(pkgRoot, "cli.bundle.mjs"), ...argv], {
    cwd: process.cwd(),
    stdio: "inherit",
    shell: false,
    env: process.env,
  });
  if (result.error) throw result.error;
  return result.status ?? 1;
}

async function runRepo(argv) {
  const actionRaw = argv[0];
  if (!actionRaw) throw new Error("repo requires map, symbol, references, or outline");
  const action = actionRaw === "sym" ? "symbol" : actionRaw === "refs" ? "references" : actionRaw;
  const { root, rest } = parseRootOption(argv.slice(1));
  let request;
  if (action === "map") {
    if (rest.length > 1) throw new Error("repo map accepts at most one positional root");
    request = { action: "map", root: root ?? rest[0] };
  } else if (action === "symbol" || action === "references") {
    if (rest.length !== 1) throw new Error(`repo ${action} requires exactly one name`);
    request = { action, query: rest[0], root };
  } else if (action === "outline") {
    if (rest.length !== 1) throw new Error("repo outline requires exactly one file");
    request = { action: "outline", path: rest[0], root };
  } else {
    throw new Error(`unknown repo action: ${actionRaw}`);
  }
  const receipt = await repoQcNative(request, {
    cwd: process.cwd(),
    packageRoot: pkgRoot,
    timeoutMs: Number(process.env.QUIET_CONTEXT_REPO_TIMEOUT_MS) || 30_000,
  });
  if (receipt.stdout) process.stdout.write(receipt.stdout);
  if (receipt.stderr) process.stderr.write(receipt.stderr);
  return receipt.exitCode;
}

async function main() {
  if (args.length === 0 || args[0] === "help" || args[0] === "--help" || args[0] === "-h") {
    help();
    return 0;
  }
  if (args[0] === "--version" || args[0] === "-V") {
    process.stdout.write(`qc ${pkg.version}\n`);
    return 0;
  }
  if (args[0] === "status") {
    const status = await verifyQcNative({ packageRoot: pkgRoot });
    process.stdout.write(`QuietContext ${pkg.version}; native ${status.nativeVersion}; protocol ${status.protocolVersion}\n`);
    return 0;
  }
  if (args[0] === "routing") {
    const action = args[1] ?? "status";
    const marker = qcBashRoutingMarkerPath();
    if (action === "status") {
      process.stdout.write(`qc Bash routing: ${qcBashRoutingEnabled() ? "enabled" : "disabled"}\n`);
      return 0;
    }
    if (action === "enable") {
      mkdirSync(dirname(marker), { recursive: true, mode: 0o700 });
      writeFileSync(marker, "enabled\n", { mode: 0o600 });
      try { chmodSync(marker, 0o600); } catch { /* Windows/no-op */ }
      process.stdout.write(`qc Bash routing enabled: ${marker}\n`);
      return 0;
    }
    if (action === "disable") {
      try { unlinkSync(marker); } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
      process.stdout.write("qc Bash routing disabled\n");
      return 0;
    }
    process.stderr.write(`qc: unknown routing action: ${action}\n`);
    return 2;
  }
  if (args[0] === "repo") {
    return runRepo(args.slice(1));
  }
  if (args[0] === "index" || args[0] === "search") {
    return runContextModeCli(args);
  }
  if (args[0] === "doctor") {
    const status = await verifyQcNative({ packageRoot: pkgRoot });
    process.stdout.write(`qc-native: OK ${status.nativeVersion} protocol=${status.protocolVersion}\n`);
    const existing = spawnSync(process.execPath, [resolve(pkgRoot, "cli.bundle.mjs"), "doctor"], {
      cwd: process.cwd(),
      stdio: "inherit",
      shell: false,
      env: process.env,
    });
    return existing.status ?? 1;
  }
  if (args[0] === "run") {
    const separator = args[1] === "--" ? 2 : 1;
    const command = args.slice(separator);
    if (command.length === 0) {
      process.stderr.write("qc: run requires a command\n");
      return 2;
    }
    const receipt = await runQcNative(command, {
      cwd: process.cwd(),
      packageRoot: pkgRoot,
      timeoutMs: Number(process.env.QUIET_CONTEXT_RUN_TIMEOUT_MS) || 24 * 60 * 60 * 1000,
    });
    if (receipt.stdout.compact) process.stdout.write(receipt.stdout.compact);
    if (receipt.stderr.compact) process.stderr.write(receipt.stderr.compact);
    return receipt.exitCode;
  }
  process.stderr.write(`qc: unknown command: ${args[0]}\n`);
  help();
  return 2;
}

main()
  .then((code) => { process.exitCode = code; })
  .catch((error) => {
    const prefix = error instanceof QcNativeError ? `${error.code}: ` : "";
    process.stderr.write(`qc: ${prefix}${error instanceof Error ? error.message : String(error)}\n`);
    process.exitCode = 1;
  });
