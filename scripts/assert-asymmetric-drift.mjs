#!/usr/bin/env node
// Issue #531 — asymmetric-drift invariant asserter.
//
// The canonical MCP server config lives in TWO source-tracked files:
//
//   1. `.mcp.json.example`                    (template; copied to .mcp.json
//                                              locally by contributors. .mcp.json
//                                              itself is .gitignored after the
//                                              #531 architectural untrack — see
//                                              .gitignore and tests/core/cli.test.ts
//                                              "package.json files[] MUST NOT ship
//                                              .mcp.json".)
//   2. `.claude-plugin/plugin.json`           (Claude Code's primary read path
//                                              for installed plugins. cli.ts
//                                              upgrade() writes a matching
//                                              .mcp.json into the plugin cache.)
//
// If the two source-tracked files drift, fresh installs break silently
// (the #253 regression survived a full release cycle because no invariant
// caught the bare `./start.mjs` shape).
//
// This script is the build-chain half of the slice-9 invariant pair.
// The vitest sibling (tests/scripts/asymmetric-drift-assert.test.ts) covers
// the source tree at test time; this script covers the build chain — wired
// into `npm run build` so any regression surfaces in CI before publish.
//
// Contract:
//   - Read `.mcp.json.example` and `.claude-plugin/plugin.json` from --root.
//   - Extract mcpServers[the plugin name].args[0] from each.
//   - Assert both equal the literal `${CLAUDE_PLUGIN_ROOT}/start.mjs`.
//   - Assert the two values are equal (the explicit drift check).
//   - If a `.mcp.json` exists (contributor's local copy), check it too —
//     but absence is fine (it's .gitignored).
//   - Exit 0 on success, 1 with a violations report on failure.
//
// Usage:
//   node scripts/assert-asymmetric-drift.mjs              # checks repo root
//   node scripts/assert-asymmetric-drift.mjs --root <dir> # checks <dir>

import { existsSync, readFileSync } from "node:fs";
import { resolve, dirname } from "node:path";
import { fileURLToPath } from "node:url";

// Canonical shape since the shared-daemon cutover: one localhost HTTP daemon
// serves every session; manifests point at it instead of spawning start.mjs.
// start.mjs + server.bundle.mjs stay required — they are the rollback path.
const CANONICAL_SERVER_ENTRY = {
  type: "http",
  url: "http://127.0.0.1:48619/mcp",
  headers: { "X-QuietContext-Root": "${PWD}" },
  headersHelper: "node ${CLAUDE_PLUGIN_ROOT}/daemon-headers.mjs",
};
const DEFAULT_PLUGIN_KEY = "context-mode";
const SKILLS_PATH = "./skills/";
const REQUIRED_PLUGIN_RUNTIME_FILES = [
  "start.mjs",
  "start-http.mjs",
  "daemon-headers.mjs",
  "server.bundle.mjs",
  "http-server.bundle.mjs",
  "cli.bundle.mjs",
];

function parseArgs(argv) {
  const out = { root: null };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--root" && i + 1 < argv.length) {
      out.root = argv[i + 1];
      i++;
    }
  }
  return out;
}

function readServerEntry(filePath, pluginKey = DEFAULT_PLUGIN_KEY) {
  if (!existsSync(filePath)) return { ok: false, error: `missing: ${filePath}` };
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(filePath, "utf-8"));
  } catch (err) {
    return { ok: false, error: `parse-failed (${filePath}): ${err && err.message}` };
  }
  const servers = parsed && parsed.mcpServers;
  if (!servers || typeof servers !== "object") {
    return { ok: false, error: `no mcpServers in ${filePath}` };
  }
  const ours = servers[pluginKey];
  if (!ours || typeof ours !== "object") {
    return { ok: false, error: `no mcpServers entry for ${pluginKey} in ${filePath}` };
  }
  return { ok: true, value: ours };
}

const stable = (v) => {
  const sort = (x) =>
    Array.isArray(x)
      ? x.map(sort)
      : x && typeof x === "object"
        ? Object.fromEntries(Object.keys(x).sort().map((k) => [k, sort(x[k])]))
        : x;
  return JSON.stringify(sort(v));
};

function readJson(filePath) {
  if (!existsSync(filePath)) return { ok: false, error: `missing: ${filePath}` };
  try {
    return { ok: true, value: JSON.parse(readFileSync(filePath, "utf-8")) };
  } catch (err) {
    return { ok: false, error: `parse-failed (${filePath}): ${err && err.message}` };
  }
}

function main() {
  const { root: explicitRoot } = parseArgs(process.argv.slice(2));
  const __dirname = dirname(fileURLToPath(import.meta.url));
  const root = explicitRoot
    ? resolve(explicitRoot)
    : resolve(__dirname, "..");

  const exampleJsonPath = resolve(root, ".mcp.json.example");
  const pluginJsonPath = resolve(root, ".claude-plugin", "plugin.json");
  const localMcpJsonPath = resolve(root, ".mcp.json");

  /** @type {string[]} */
  const violations = [];

  const pluginJson = readJson(pluginJsonPath);
  const pluginKey = pluginJson.ok && typeof pluginJson.value?.name === "string"
    ? pluginJson.value.name
    : DEFAULT_PLUGIN_KEY;
  const example = readServerEntry(exampleJsonPath, pluginKey);
  const plg = readServerEntry(pluginJsonPath, pluginKey);

  if (!example.ok) violations.push(example.error);
  if (!plg.ok) violations.push(plg.error);

  const canonical = stable(CANONICAL_SERVER_ENTRY);
  if (example.ok && stable(example.value) !== canonical) {
    violations.push(
      `.mcp.json.example mcpServers.${pluginKey} is ${stable(example.value)} but must equal ${canonical}. ` +
        `Contributors copy this template to .mcp.json for local dev, so the template MUST hold the canonical form. (Issue #531 / #253 class.)`,
    );
  }
  if (plg.ok && stable(plg.value) !== canonical) {
    violations.push(
      `.claude-plugin/plugin.json mcpServers.${pluginKey} is ${stable(plg.value)} but must equal ${canonical}. (Issue #523 class.)`,
    );
  }
  if (example.ok && plg.ok && stable(example.value) !== stable(plg.value)) {
    violations.push(
      `asymmetric drift: .mcp.json.example vs .claude-plugin/plugin.json mcpServers.${pluginKey} disagree. The two source-tracked manifests MUST agree so contributors copying the template and end-users via marketplace install reach the same daemon.`,
    );
  }

  if (pluginJson.ok) {
    const skills = pluginJson.value && pluginJson.value.skills;
    if (skills !== SKILLS_PATH) {
      violations.push(
        `.claude-plugin/plugin.json skills is "${skills}" but must equal "${SKILLS_PATH}". The npm package ships top-level skills/, not .claude/skills/.`,
      );
    }
    if (!existsSync(resolve(root, "skills"))) {
      violations.push(`missing skills directory at ${resolve(root, "skills")}`);
    }
  } else {
    violations.push(pluginJson.error);
  }

  for (const rel of REQUIRED_PLUGIN_RUNTIME_FILES) {
    if (!existsSync(resolve(root, rel))) {
      violations.push(
        `missing plugin runtime file at ${resolve(root, rel)}. ` +
          `.claude-plugin/plugin.json can load but the MCP server will expose zero tools if ${rel} is absent.`,
      );
    }
  }

  // Contributor's local .mcp.json (if present) — must match the template.
  // Absence is fine; the file is .gitignored after the #531 architectural untrack.
  if (existsSync(localMcpJsonPath)) {
    const local = readServerEntry(localMcpJsonPath, pluginKey);
    if (local.ok && stable(local.value) !== canonical) {
      violations.push(
        `local .mcp.json mcpServers.${pluginKey} does not match the canonical HTTP entry. ` +
          `If you intentionally use a relative dev path locally, ignore — but this file would ship the regression if it ever lands in package.json files[]. Consider \`cp .mcp.json.example .mcp.json\` to reset.`,
      );
    }
  }

  if (violations.length > 0) {
    process.stderr.write("asymmetric-drift: FAIL\n");
    for (const v of violations) {
      process.stderr.write(`  - ${v}\n`);
    }
    process.exit(1);
  }

  process.stdout.write(
    `asymmetric-drift: OK (.mcp.json.example + .claude-plugin/plugin.json both pin the canonical HTTP daemon entry; plugin skills path is ${SKILLS_PATH}; runtime files present)\n`,
  );
}

main();
