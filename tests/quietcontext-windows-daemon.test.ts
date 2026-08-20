import { afterEach, describe, expect, test } from "vitest";
import { createServer, type Server } from "node:net";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  DEFAULT_PORT,
  PLUGIN_KEY,
  claudeConfigDir,
  isPortInUse,
  pluginRootFromCacheScan,
  pluginRootFromManifest,
  readPort,
  resolvePluginRoot,
} from "../windows/quietcontext-daemon.mjs";

const cleanups: Array<() => void> = [];

afterEach(() => {
  while (cleanups.length) cleanups.pop()!();
});

function makeConfigDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "quietcontext-win-"));
  cleanups.push(() => rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function makePluginVersion(configDir: string, version: string, withEntrypoint = true): string {
  const dir = join(configDir, "plugins", "cache", "quietcontext", "quietcontext", version);
  mkdirSync(dir, { recursive: true });
  if (withEntrypoint) writeFileSync(join(dir, "start-http.mjs"), "");
  return dir;
}

function writeManifest(configDir: string, installPaths: string[]): void {
  const dir = join(configDir, "plugins");
  mkdirSync(dir, { recursive: true });
  writeFileSync(
    join(dir, "installed_plugins.json"),
    JSON.stringify({
      version: 2,
      plugins: { [PLUGIN_KEY]: installPaths.map((installPath) => ({ scope: "user", installPath })) },
    }),
  );
}

function listenOnEphemeralPort(): Promise<{ port: number; server: Server }> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      if (address === null || typeof address === "string") {
        reject(new Error("expected a TCP address"));
        return;
      }
      resolve({ port: address.port, server });
    });
  });
}

describe("claudeConfigDir", () => {
  test("defaults to ~/.claude", () => {
    expect(claudeConfigDir({}, join("/home", "someone"))).toBe(join("/home", "someone", ".claude"));
  });

  test("honors CLAUDE_CONFIG_DIR", () => {
    const configured = join(tmpdir(), "explicit-config");
    expect(claudeConfigDir({ CLAUDE_CONFIG_DIR: configured }, "/home/someone")).toBe(configured);
  });

  test("expands a leading tilde in CLAUDE_CONFIG_DIR", () => {
    const home = join(tmpdir(), "home-dir");
    expect(claudeConfigDir({ CLAUDE_CONFIG_DIR: "~/nested/config" }, home)).toBe(join(home, "nested", "config"));
  });

  test("ignores a blank CLAUDE_CONFIG_DIR", () => {
    const home = join(tmpdir(), "home-dir");
    expect(claudeConfigDir({ CLAUDE_CONFIG_DIR: "   " }, home)).toBe(join(home, ".claude"));
  });
});

describe("plugin root resolution", () => {
  test("reads the install path recorded in installed_plugins.json", () => {
    const configDir = makeConfigDir();
    const installed = makePluginVersion(configDir, "1.0.169");
    writeManifest(configDir, [installed]);

    expect(pluginRootFromManifest(configDir)).toBe(installed);
  });

  test("skips manifest entries whose directory no longer has the entrypoint", () => {
    const configDir = makeConfigDir();
    const stale = join(configDir, "plugins", "cache", "quietcontext", "quietcontext", "1.0.100");
    const current = makePluginVersion(configDir, "1.0.169");
    writeManifest(configDir, [stale, current]);

    expect(pluginRootFromManifest(configDir)).toBe(current);
  });

  test("returns null when no manifest exists", () => {
    expect(pluginRootFromManifest(makeConfigDir())).toBeNull();
  });

  test("returns null on a corrupt manifest", () => {
    const configDir = makeConfigDir();
    mkdirSync(join(configDir, "plugins"), { recursive: true });
    writeFileSync(join(configDir, "plugins", "installed_plugins.json"), "{ not json");

    expect(pluginRootFromManifest(configDir)).toBeNull();
  });

  test("cache scan picks the highest version that has the entrypoint", () => {
    const configDir = makeConfigDir();
    makePluginVersion(configDir, "1.0.9");
    const usable = makePluginVersion(configDir, "1.0.10");
    makePluginVersion(configDir, "1.0.11", false);

    expect(pluginRootFromCacheScan(configDir)).toBe(usable);
  });

  test("cache scan returns null when the cache is absent", () => {
    expect(pluginRootFromCacheScan(makeConfigDir())).toBeNull();
  });

  test("manifest wins over the cache scan", () => {
    const configDir = makeConfigDir();
    const older = makePluginVersion(configDir, "1.0.9");
    makePluginVersion(configDir, "1.0.169");
    writeManifest(configDir, [older]);

    expect(resolvePluginRoot(configDir)).toBe(older);
  });

  test("falls back to the cache scan when the manifest yields nothing", () => {
    const configDir = makeConfigDir();
    const current = makePluginVersion(configDir, "1.0.169");

    expect(resolvePluginRoot(configDir)).toBe(current);
  });
});

describe("readPort", () => {
  test("defaults to the daemon port", () => {
    expect(readPort({})).toBe(DEFAULT_PORT);
    expect(readPort({ QUIET_CONTEXT_DAEMON_PORT: "" })).toBe(DEFAULT_PORT);
  });

  test("accepts an explicit port", () => {
    expect(readPort({ QUIET_CONTEXT_DAEMON_PORT: "5000" })).toBe(5000);
  });

  test.each(["0", "70000", "abc", "48619.5"])("rejects %s", (value) => {
    expect(() => readPort({ QUIET_CONTEXT_DAEMON_PORT: value })).toThrow(/invalid QUIET_CONTEXT_DAEMON_PORT/);
  });
});

describe("isPortInUse", () => {
  test("detects a listening socket", async () => {
    const { port, server } = await listenOnEphemeralPort();
    try {
      await expect(isPortInUse(port)).resolves.toBe(true);
    } finally {
      await new Promise((done) => server.close(done));
    }
  });

  test("reports a closed port as free", async () => {
    const { port, server } = await listenOnEphemeralPort();
    await new Promise((done) => server.close(done));

    await expect(isPortInUse(port)).resolves.toBe(false);
  });
});
