import { readFileSync } from "node:fs";

const server = readFileSync(new URL("../src/server.ts", import.meta.url), "utf8");
const readme = readFileSync(new URL("../README.md", import.meta.url), "utf8");
const failures = [];

for (const forbidden of ["TOKEN_SAVING_TOOLS", "PUBLIC_TOOL_NAMES", ").registerTool ="]) {
  if (server.includes(forbidden)) failures.push(`src/server.ts contains ${forbidden}`);
}

const registrations = [...server.matchAll(/registerQuietTool\(\s*"([^"]+)"/g)].map((match) => match[1]);
const expected = ["execute", "exec-file", "index", "search", "fetch-index", "batch"];
if (JSON.stringify(registrations) !== JSON.stringify(expected)) {
  failures.push(`quiet tool registrations: ${registrations.join(", ")}`);
}
if (!readme.includes("MCP `tools/list`: ≤4 KiB")) failures.push("README omits tools/list budget");

if (failures.length > 0) {
  console.error(failures.join("\n"));
  process.exit(1);
}
console.log("quiet-lint: OK");
