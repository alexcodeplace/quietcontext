#!/usr/bin/env node
// Launcher for the shared QuietContext HTTP daemon under Windows Task
// Scheduler. Resolves the installed plugin's current version directory on
// every start, so a plugin update does not strand the registered task on a
// version path that no longer exists.
import { get as httpGet } from "node:http";
import { createConnection } from "node:net";
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const PLUGIN_KEY = "quietcontext@quietcontext";
export const DEFAULT_PORT = 48619;

export function claudeConfigDir(env = process.env, home = homedir()) {
  const configured = env.CLAUDE_CONFIG_DIR;
  if (configured && configured.trim() !== "") {
    return configured.startsWith("~")
      ? resolve(home, configured.replace(/^~[/\\]?/, ""))
      : resolve(configured);
  }
  return join(home, ".claude");
}

function compareVersions(a, b) {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) - (pb[i] || 0);
  }
  return 0;
}

function hasEntrypoint(dir) {
  return existsSync(join(dir, "start-http.mjs"));
}

export function pluginRootFromManifest(configDir) {
  const manifest = join(configDir, "plugins", "installed_plugins.json");
  if (!existsSync(manifest)) return null;
  let parsed;
  try {
    parsed = JSON.parse(readFileSync(manifest, "utf8"));
  } catch {
    return null;
  }
  for (const entry of parsed?.plugins?.[PLUGIN_KEY] ?? []) {
    const installPath = entry?.installPath;
    if (installPath && hasEntrypoint(installPath)) return installPath;
  }
  return null;
}

export function pluginRootFromCacheScan(configDir) {
  const [plugin, marketplace] = PLUGIN_KEY.split("@");
  const parent = join(configDir, "plugins", "cache", marketplace, plugin);
  if (!existsSync(parent)) return null;
  const versions = readdirSync(parent)
    .filter((name) => /^\d+\.\d+/.test(name) && statSync(join(parent, name)).isDirectory())
    .sort(compareVersions);
  for (let i = versions.length - 1; i >= 0; i--) {
    const candidate = join(parent, versions[i]);
    if (hasEntrypoint(candidate)) return candidate;
  }
  return null;
}

export function resolvePluginRoot(configDir) {
  return pluginRootFromManifest(configDir) ?? pluginRootFromCacheScan(configDir);
}

export function isPortInUse(port, host = "127.0.0.1", timeoutMs = 1500) {
  return new Promise((settle) => {
    const socket = createConnection({ host, port });
    const finish = (inUse) => {
      socket.destroy();
      settle(inUse);
    };
    socket.on("connect", () => finish(true));
    socket.on("error", () => finish(false));
    socket.setTimeout(timeoutMs, () => finish(false));
  });
}

export function isQuietContextHealthy(port, host = "127.0.0.1", timeoutMs = 1500) {
  return new Promise((settle) => {
    let settled = false;
    let request;
    const finish = (healthy) => {
      if (settled) return;
      settled = true;
      request?.destroy();
      settle(healthy);
    };

    request = httpGet(
      { host, port, path: "/healthz", headers: { accept: "application/json" } },
      (response) => {
        let body = "";
        response.setEncoding("utf8");
        response.on("data", (chunk) => {
          if (body.length < 4096) body += chunk;
        });
        response.on("end", () => {
          if (response.statusCode !== 200) return finish(false);
          try {
            const health = JSON.parse(body);
            finish(health?.ok === true && health?.name === "quietcontext");
          } catch {
            finish(false);
          }
        });
      },
    );
    request.on("error", () => finish(false));
    request.setTimeout(timeoutMs, () => finish(false));
  });
}

export function readPort(env = process.env) {
  const raw = env.QUIET_CONTEXT_DAEMON_PORT;
  if (raw === undefined || raw === "") return DEFAULT_PORT;
  const port = Number(raw);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`invalid QUIET_CONTEXT_DAEMON_PORT: ${raw}`);
  }
  return port;
}

export async function main() {
  let port;
  try {
    port = readPort();
  } catch (err) {
    console.error(`[quietcontext-launcher] ${err.message}`);
    return 1;
  }

  if (await isPortInUse(port)) {
    if (await isQuietContextHealthy(port)) {
      console.error(`[quietcontext-launcher] QuietContext already serving on 127.0.0.1:${port} — nothing to start`);
      return 0;
    }
    console.error(`[quietcontext-launcher] 127.0.0.1:${port} is occupied by another service — refusing to report success`);
    return 1;
  }

  const configDir = claudeConfigDir();
  const pluginRoot = resolvePluginRoot(configDir);
  if (!pluginRoot) {
    console.error(`[quietcontext-launcher] ${PLUGIN_KEY} not installed under ${configDir}`);
    return 1;
  }

  process.env.QUIET_CONTEXT_DAEMON_PORT = String(port);
  await import(pathToFileURL(join(pluginRoot, "start-http.mjs")).href);
  return 0;
}

const entrypoint = process.argv[1] ? resolve(process.argv[1]) : "";
if (entrypoint === fileURLToPath(import.meta.url)) {
  const code = await main();
  if (code !== 0) process.exit(code);
}
