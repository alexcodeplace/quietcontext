import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const dataDir = mkdtempSync(join(tmpdir(), "quietcontext-economics-"));
const child = spawn(process.execPath, [join(root, "start.mjs")], {
  cwd: root,
  env: { ...process.env, QUIET_CONTEXT_PLATFORM: "codex", QUIET_CONTEXT_DIR: dataDir },
});

let nextId = 1;
let stdout = "";
const pending = new Map<number, (response: Record<string, any>) => void>();

child.stdout.on("data", (chunk: Buffer) => {
  stdout += chunk.toString();
  for (;;) {
    const newline = stdout.indexOf("\n");
    if (newline < 0) break;
    const line = stdout.slice(0, newline).trim();
    stdout = stdout.slice(newline + 1);
    if (!line) continue;
    const response = JSON.parse(line) as Record<string, any>;
    pending.get(Number(response.id))?.(response);
    pending.delete(Number(response.id));
  }
});

function call(method: string, params: Record<string, unknown>): Promise<Record<string, any>> {
  const id = nextId++;
  const response = new Promise<Record<string, any>>((resolveResponse) => {
    pending.set(id, resolveResponse);
  });
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
  return response;
}

function bytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value));
}

function text(response: Record<string, any>): string {
  if (response.error) throw new Error(JSON.stringify(response.error));
  return response.result?.content?.[0]?.text ?? "";
}

function assertBudget(name: string, actual: number, limit: number): void {
  if (actual > limit) throw new Error(`${name}: ${actual} > ${limit} bytes`);
}

try {
  await call("initialize", {
    protocolVersion: "2024-11-05",
    capabilities: {},
    clientInfo: { name: "quietcontext-economics", version: "1.0.0" },
  });
  child.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized", params: {} }) + "\n");

  const listed = await call("tools/list", {});
  const toolBytes = bytes(listed.result?.tools ?? []);

  const execArgs = {
    language: "javascript",
    code: `// request-argument-marker\n${"// input\n".repeat(250)}console.log("OK")`,
  };
  const executed = await call("tools/call", { name: "execute", arguments: execArgs });
  const execText = text(executed);
  const scriptRef = execText.match(/\[s:([a-f0-9]+)\]/)?.[1];
  if (!scriptRef) throw new Error("execute response omitted script reference");
  const repeatArgs = { script_ref: scriptRef };
  const repeated = await call("tools/call", { name: "execute", arguments: repeatArgs });
  if (!text(repeated).includes("OK")) throw new Error("script reference execution failed");

  await call("tools/call", {
    name: "index",
    arguments: {
      source: "economics-fixture",
      content: "# Token economics\n\neconomics-marker bounded response",
    },
  });
  const searchArgs = { queries: ["economics-marker"] };
  const searched = await call("tools/call", { name: "search", arguments: searchArgs });
  const searchText = text(searched);

  assertBudget("tools/list", toolBytes, 4 * 1024);
  assertBudget("exec response", Buffer.byteLength(execText), 8 * 1024);
  assertBudget("search response", Buffer.byteLength(searchText), 8 * 1024);
  if (execText.includes("request-argument-marker")) throw new Error("exec echoed request source");
  if (bytes(repeatArgs) >= bytes(execArgs)) throw new Error("script reference did not reduce request arguments");

  const metrics = [
    ["tools/list", toolBytes],
    ["source args", bytes(execArgs)],
    ["source response", bytes(executed.result)],
    ["ref args", bytes(repeatArgs)],
    ["ref response", bytes(repeated.result)],
    ["search args", bytes(searchArgs)],
    ["search response", bytes(searched.result)],
  ] as const;
  const total = metrics.reduce((sum, [, value]) => sum + value, 0);
  for (const [name, value] of metrics) {
    console.log(`${name.padEnd(18)} ${String(value).padStart(5)} B  ~${Math.ceil(value / 4)} tokens`);
  }
  console.log(`${"scenario total".padEnd(18)} ${String(total).padStart(5)} B  ~${Math.ceil(total / 4)} tokens`);
} finally {
  child.kill();
  rmSync(dataDir, { recursive: true, force: true });
}
