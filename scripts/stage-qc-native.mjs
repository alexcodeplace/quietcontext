#!/usr/bin/env node
import { createHash } from "node:crypto";
import { chmodSync, copyFileSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pkg = JSON.parse(readFileSync(resolve(root, "package.json"), "utf8"));
const key = process.platform === "linux" && process.arch === "x64"
  ? "linux-x64"
  : process.platform === "darwin" && process.arch === "x64"
    ? "darwin-x64"
    : process.platform === "darwin" && process.arch === "arm64"
      ? "darwin-arm64"
      : process.platform === "win32" && process.arch === "x64"
        ? "win32-x64"
        : null;
if (!key) {
  console.error(`qc-native: unsupported staging target ${process.platform}-${process.arch}`);
  process.exit(2);
}

const prebuilt = process.env.QUIET_CONTEXT_NATIVE_PREBUILT;
let source;
if (prebuilt) {
  source = resolve(prebuilt);
  if (!existsSync(source)) {
    console.error(`qc-native: prebuilt artifact does not exist: ${source}`);
    process.exit(2);
  }
} else {
  const cargo = process.env.CARGO || "cargo";
  const manifest = resolve(root, "native", "qc", "Cargo.toml");
  const build = spawnSync(cargo, ["build", "--release", "--locked", "--manifest-path", manifest], {
    cwd: root,
    stdio: "inherit",
    shell: false,
  });
  if (build.status !== 0) process.exit(build.status ?? 1);
  source = resolve(root, "native", "qc", "target", "release", process.platform === "win32" ? "qc-native.exe" : "qc-native");
}
const destDir = resolve(root, "vendor", "qc-native", key);
const dest = resolve(destDir, process.platform === "win32" ? "qc-native.exe" : "qc-native");
rmSync(destDir, { recursive: true, force: true });
mkdirSync(destDir, { recursive: true });
copyFileSync(source, dest);
if (process.platform !== "win32") chmodSync(dest, 0o755);

const probe = spawnSync(dest, ["status"], { cwd: root, encoding: "utf8", shell: false });
if (probe.status !== 0) {
  process.stderr.write(probe.stderr || "qc-native status failed\n");
  process.exit(probe.status ?? 1);
}
let status;
try { status = JSON.parse(probe.stdout.trim()); } catch { status = null; }
if (!status || status.protocolVersion !== 1 || typeof status.nativeVersion !== "string" || status.product !== "QuietContext") {
  console.error("qc-native: staged binary returned incompatible status");
  process.exit(1);
}
const sha256 = createHash("sha256").update(readFileSync(dest)).digest("hex");
writeFileSync(resolve(destDir, "manifest.json"), JSON.stringify({
  schemaVersion: 1,
  packageVersion: pkg.version,
  nativeVersion: status.nativeVersion,
  protocolVersion: status.protocolVersion,
  platform: process.platform,
  arch: process.arch,
  sha256,
}, null, 2) + "\n");
console.log(`qc-native staged ${key} ${status.nativeVersion} sha256:${sha256}`);
