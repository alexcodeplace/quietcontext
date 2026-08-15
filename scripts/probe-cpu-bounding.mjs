#!/usr/bin/env node
/**
 * Item 5 measurement probe — execute-child CPU bounding.
 *
 * Two roots against one shared HTTP daemon:
 *   - "heavy" root: a continuous stream of CPU-bound `execute` calls,
 *     enough concurrently in flight to contend for cores.
 *   - "cheap" root: sequential trivial `execute` calls, timed round-trip.
 *
 * Reports p50/p99 latency of the cheap-root calls while the heavy root is
 * running, plus os.loadavg()/cpus().length so the numbers can be read in
 * their load context. Not a vitest test — a manual before/after
 * measurement tool (see docs/plans or the item 5 commit message for the
 * numbers this produced).
 *
 * Usage: node scripts/probe-cpu-bounding.mjs
 */
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir, cpus, loadavg } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(fileURLToPath(import.meta.url), "..", "..");

const HEAVY_CONCURRENCY = Math.max(8, cpus().length + cpus().length + cpus().length);
const CHEAP_SAMPLES = 60;
const PROBE_DURATION_MS = 15_000;

function startDaemon(env) {
  return new Promise((resolvePort, reject) => {
    const daemon = spawn(process.execPath, [join(ROOT, "start-http.mjs")], {
      cwd: ROOT,
      env: { ...process.env, ...env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stderr = "";
    const onData = (chunk) => {
      stderr += chunk.toString();
      const m = /listening on http:\/\/127\.0\.0\.1:(\d+)\/mcp/.exec(stderr);
      if (m) {
        daemon.stderr.off("data", onData);
        resolvePort({ daemon, port: Number(m[1]) });
      }
    };
    daemon.stderr.on("data", onData);
    daemon.once("exit", (code) => reject(new Error(`daemon exited early (${code}): ${stderr}`)));
    daemon.once("error", reject);
  });
}

async function callTool(port, token, root, name, args) {
  const started = performance.now();
  const res = await fetch(`http://127.0.0.1:${port}/mcp`, {
    method: "POST",
    headers: {
      "content-type": "application/json",
      accept: "application/json, text/event-stream",
      authorization: `Bearer ${token}`,
      "x-quietcontext-root": root,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: Math.floor(Math.random() * 1e9),
      method: "tools/call",
      params: { name, arguments: args },
    }),
  });
  await res.text();
  return performance.now() - started;
}

function percentile(sorted, p) {
  const idx = Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length));
  return sorted[idx];
}

async function main() {
  const scratch = mkdtempSync(join(tmpdir(), "qc-cpu-probe-"));
  const heavyRoot = join(scratch, "heavy");
  const cheapRoot = join(scratch, "cheap");
  mkdirSync(heavyRoot, { recursive: true });
  mkdirSync(cheapRoot, { recursive: true });
  const tokenFile = join(scratch, "daemon.token");

  const { daemon, port } = await startDaemon({
    QUIET_CONTEXT_DAEMON_PORT: "0",
    QUIET_CONTEXT_DAEMON_TOKEN_FILE: tokenFile,
    QUIET_CONTEXT_DIR: join(scratch, "data"),
  });
  const token = readFileSync(tokenFile, "utf8").trim();

  console.log(`cpus=${cpus().length} loadavg(1m,5m,15m)=${loadavg().map((n) => n.toFixed(2)).join(",")}`);
  console.log(`heavy concurrency=${HEAVY_CONCURRENCY} probe duration=${PROBE_DURATION_MS}ms`);

  const deadline = Date.now() + PROBE_DURATION_MS;
  let stopHeavy = false;

  // Keep HEAVY_CONCURRENCY CPU-bound calls in flight against heavyRoot for
  // the whole probe window — each call burns CPU for ~250ms computing
  // primes, then immediately re-fires.
  const heavyCode = `
    const start = Date.now();
    let n = 2, count = 0;
    while (Date.now() - start < 250) {
      let isPrime = true;
      for (let d = 2; d * d <= n; d++) { if (n % d === 0) { isPrime = false; break; } }
      if (isPrime) count++;
      n++;
    }
    console.log(count);
  `;
  async function heavyWorker() {
    while (!stopHeavy) {
      await callTool(port, token, heavyRoot, "execute", { language: "javascript", code: heavyCode });
    }
  }
  const heavyWorkers = Array.from({ length: HEAVY_CONCURRENCY }, () => heavyWorker());

  // Let the heavy load ramp up before sampling cheap-call latency.
  await new Promise((r) => setTimeout(r, 1000));

  const cheapLatencies = [];
  while (Date.now() < deadline && cheapLatencies.length < CHEAP_SAMPLES) {
    const ms = await callTool(port, token, cheapRoot, "execute", {
      language: "javascript",
      code: "console.log(1)",
    });
    cheapLatencies.push(ms);
  }

  stopHeavy = true;
  await Promise.all(heavyWorkers);

  cheapLatencies.sort((a, b) => a - b);
  const p50 = percentile(cheapLatencies, 50);
  const p99 = percentile(cheapLatencies, 99);
  const max = cheapLatencies[cheapLatencies.length - 1];

  console.log(`cheap-call samples=${cheapLatencies.length}`);
  console.log(`cheap-call latency p50=${p50.toFixed(1)}ms p99=${p99.toFixed(1)}ms max=${max.toFixed(1)}ms`);

  daemon.kill("SIGTERM");
  await new Promise((r) => setTimeout(r, 200));
  rmSync(scratch, { recursive: true, force: true });
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
