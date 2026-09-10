import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { mkdtempSync, rmSync } from "node:fs";
import { afterEach, describe, expect, test } from "vitest";
import { qcNativePlatformKey, resolveQcNativeArtifact, QcNativeError } from "../src/native-qc.js";

const scratch: string[] = [];
afterEach(() => {
  for (const dir of scratch.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function fixture(overrides: Record<string, unknown> = {}) {
  const root = mkdtempSync(join(tmpdir(), "qc-native-contract-"));
  scratch.push(root);
  writeFileSync(join(root, "package.json"), JSON.stringify({ version: "9.8.7" }));
  const dir = join(root, "vendor", "qc-native", "linux-x64");
  mkdirSync(dir, { recursive: true });
  const binary = join(dir, "qc-native");
  writeFileSync(binary, "fixture-native");
  chmodSync(binary, 0o755);
  const sha256 = createHash("sha256").update("fixture-native").digest("hex");
  writeFileSync(join(dir, "manifest.json"), JSON.stringify({
    schemaVersion: 1,
    packageVersion: "9.8.7",
    nativeVersion: "0.1.0",
    protocolVersion: 1,
    platform: "linux",
    arch: "x64",
    sha256,
    ...overrides,
  }));
  return { root, binary };
}

describe("QuietContext native artifact contract", () => {
  test("maps only explicit supported platform/arch pairs", () => {
    expect(qcNativePlatformKey("linux", "x64")).toBe("linux-x64");
    expect(qcNativePlatformKey("darwin", "arm64")).toBe("darwin-arm64");
    expect(qcNativePlatformKey("linux", "arm64")).toBeNull();
  });

  test("accepts a package-pinned artifact with exact hash", () => {
    const { root, binary } = fixture();
    expect(resolveQcNativeArtifact({ packageRoot: root, platform: "linux", arch: "x64" }).binaryPath).toBe(binary);
  });

  test("fails closed on package-version and hash drift", () => {
    const badVersion = fixture({ packageVersion: "9.8.6" });
    expect(() => resolveQcNativeArtifact({ packageRoot: badVersion.root, platform: "linux", arch: "x64" }))
      .toThrowError(expect.objectContaining<QcNativeError>({ code: "package-version-mismatch" }));

    const badHash = fixture({ sha256: "0".repeat(64) });
    expect(() => resolveQcNativeArtifact({ packageRoot: badHash.root, platform: "linux", arch: "x64" }))
      .toThrowError(expect.objectContaining<QcNativeError>({ code: "hash-mismatch" }));
  });

  test("never treats a PATH name as a native override", () => {
    const { root } = fixture();
    expect(() => resolveQcNativeArtifact({
      packageRoot: root,
      env: { QUIET_CONTEXT_NATIVE_BIN: "qc-native", QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE: "1" },
    })).toThrowError(expect.objectContaining<QcNativeError>({ code: "missing-artifact" }));
  });
});
