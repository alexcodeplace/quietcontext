#!/usr/bin/env node
// QuietContext shared HTTP daemon launcher (systemd --user entrypoint).
// Must set the embedded flag BEFORE the bundle loads so server.ts skips its
// stdio main() (lifecycle guard would otherwise exit on closed stdin).
process.env.QUIET_CONTEXT_EMBEDDED_PLUGIN_TOOLS = "1";

import { existsSync } from "node:fs";
import { fileURLToPath } from "node:url";

const bundleUrl = new URL("./http-server.bundle.mjs", import.meta.url);
if (!existsSync(fileURLToPath(bundleUrl))) {
  console.error("[quietcontext-daemon] http-server.bundle.mjs missing — run `npm run build` first");
  process.exit(1);
}
const mod = await import(bundleUrl.href);

const portRaw = process.env.QUIET_CONTEXT_DAEMON_PORT;
const port = portRaw !== undefined && portRaw !== "" ? Number(portRaw) : undefined;
if (port !== undefined && (!Number.isInteger(port) || port < 0 || port > 65535)) {
  console.error(`[quietcontext-daemon] invalid QUIET_CONTEXT_DAEMON_PORT: ${portRaw}`);
  process.exit(1);
}

const handle = await mod.startHttpDaemon({
  port,
  tokenFile: process.env.QUIET_CONTEXT_DAEMON_TOKEN_FILE || undefined,
});

let shuttingDown = false;
const shutdown = async () => {
  if (shuttingDown) return;
  shuttingDown = true;
  try { await handle.close(); } catch { /* best effort */ }
  try { mod.releaseProcessResources(); } catch { /* best effort */ }
  process.exit(0);
};
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);
