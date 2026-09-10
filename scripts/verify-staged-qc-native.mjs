#!/usr/bin/env node
import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";

const root = resolve(import.meta.dirname, "..");
const pkg = JSON.parse(readFileSync(resolve(root, "package.json"), "utf8"));
const requested = process.argv.slice(2);
const keys = requested.length > 0 ? requested : [
  process.platform === "win32" && process.arch === "x64" ? "win32-x64" :
  process.platform === "linux" && process.arch === "x64" ? "linux-x64" : "",
].filter(Boolean);

const targets = {
  "linux-x64": { platform: "linux", arch: "x64", binary: "qc-native" },
  "win32-x64": { platform: "win32", arch: "x64", binary: "qc-native.exe" },
};

if (keys.length === 0) {
  console.error("qc-native verify: no target key supplied and host target is unsupported");
  process.exit(2);
}

for (const key of keys) {
  const target = targets[key];
  if (!target) {
    console.error(`qc-native verify: unsupported target key ${key}`);
    process.exit(2);
  }
  const dir = resolve(root, "vendor", "qc-native", key);
  const binaryPath = resolve(dir, target.binary);
  const manifestPath = resolve(dir, "manifest.json");
  if (!existsSync(binaryPath) || !existsSync(manifestPath)) {
    console.error(`qc-native verify: missing staged files for ${key}`);
    process.exit(1);
  }
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  const expected = {
    schemaVersion: 1,
    packageVersion: pkg.version,
    protocolVersion: 1,
    platform: target.platform,
    arch: target.arch,
  };
  for (const [field, value] of Object.entries(expected)) {
    if (manifest[field] !== value) {
      console.error(`qc-native verify: ${key} ${field}=${JSON.stringify(manifest[field])}, expected ${JSON.stringify(value)}`);
      process.exit(1);
    }
  }
  if (typeof manifest.nativeVersion !== "string" || manifest.nativeVersion.length === 0) {
    console.error(`qc-native verify: ${key} nativeVersion missing`);
    process.exit(1);
  }
  const sha256 = createHash("sha256").update(readFileSync(binaryPath)).digest("hex");
  if (manifest.sha256 !== sha256) {
    console.error(`qc-native verify: ${key} sha256 mismatch`);
    process.exit(1);
  }
  console.log(`qc-native verify: OK ${key} native=${manifest.nativeVersion} sha256=${sha256}`);
}
