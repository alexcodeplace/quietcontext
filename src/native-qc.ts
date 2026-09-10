import { createHash } from "node:crypto";
import { existsSync, readFileSync } from "node:fs";
import { spawn } from "node:child_process";
import { dirname, isAbsolute, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export const QC_NATIVE_PROTOCOL_VERSION = 1;
const MAX_NATIVE_STDOUT_BYTES = 256 * 1024;
const MAX_NATIVE_STDERR_BYTES = 64 * 1024;

export interface QcNativeManifest {
  schemaVersion: 1;
  packageVersion: string;
  nativeVersion: string;
  protocolVersion: number;
  platform: string;
  arch: string;
  sha256: string;
}

export interface QcNativeStreamReceipt {
  compact: string;
  rawPath: string;
  rawBytes: number;
  rawComplete: boolean;
  sampleComplete: boolean;
  compactBytes: number;
  capped: boolean;
}

export interface QcNativeRunReceipt {
  protocolVersion: number;
  nativeVersion: string;
  kind: "run";
  command: string[];
  exitCode: number;
  filter: string;
  filterInputBytes: number;
  filtered: boolean;
  deduped: boolean;
  stdout: QcNativeStreamReceipt;
  stderr: QcNativeStreamReceipt;
}

export interface QcNativeRepoReceipt {
  protocolVersion: number;
  nativeVersion: string;
  kind: "repo";
  operation: "map" | "sym" | "refs" | "outline";
  root: string;
  query?: string;
  stdout: string;
  stderr: string;
  exitCode: number;
  generation: number;
  cacheState: string;
  fallbackReason?: string;
  timings: {
    queueUs: number;
    reconcileUs: number;
    renderUs: number;
    totalUs: number;
  };
  totalLatencyUs: number;
}

interface NativeStatus {
  protocolVersion: number;
  nativeVersion: string;
  product: string;
}

export class QcNativeError extends Error {
  constructor(
    message: string,
    public readonly code:
      | "unsupported-platform"
      | "missing-artifact"
      | "invalid-manifest"
      | "package-version-mismatch"
      | "hash-mismatch"
      | "protocol-mismatch"
      | "native-version-mismatch"
      | "spawn-failed"
      | "timeout"
      | "oversized-response"
      | "invalid-response",
  ) {
    super(message);
    this.name = "QcNativeError";
  }
}

function packageRootFromModule(): string {
  const dir = dirname(fileURLToPath(import.meta.url));
  return existsSync(resolve(dir, "package.json")) ? dir : dirname(dir);
}

export function qcNativePlatformKey(
  platformName: NodeJS.Platform = process.platform,
  archName: string = process.arch,
): string | null {
  if (platformName === "linux" && archName === "x64") return "linux-x64";
  if (platformName === "darwin" && archName === "x64") return "darwin-x64";
  if (platformName === "darwin" && archName === "arm64") return "darwin-arm64";
  if (platformName === "win32" && archName === "x64") return "win32-x64";
  return null;
}

function parseJsonObject(text: string): Record<string, unknown> {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch (error) {
    throw new QcNativeError(
      `native response is not valid JSON: ${error instanceof Error ? error.message : String(error)}`,
      "invalid-response",
    );
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new QcNativeError("native response is not a JSON object", "invalid-response");
  }
  return value as Record<string, unknown>;
}

function readPackageVersion(packageRoot: string): string {
  const path = resolve(packageRoot, "package.json");
  const parsed = parseJsonObject(readFileSync(path, "utf8"));
  if (typeof parsed.version !== "string" || parsed.version.length === 0) {
    throw new QcNativeError(`package.json has no valid version: ${path}`, "invalid-manifest");
  }
  return parsed.version;
}

function validateManifest(value: Record<string, unknown>, path: string): QcNativeManifest {
  const valid =
    value.schemaVersion === 1 &&
    typeof value.packageVersion === "string" &&
    typeof value.nativeVersion === "string" &&
    typeof value.protocolVersion === "number" &&
    typeof value.platform === "string" &&
    typeof value.arch === "string" &&
    typeof value.sha256 === "string" &&
    /^[a-f0-9]{64}$/.test(value.sha256);
  if (!valid) {
    throw new QcNativeError(`invalid native manifest: ${path}`, "invalid-manifest");
  }
  return value as unknown as QcNativeManifest;
}

export interface QcNativeArtifact {
  binaryPath: string;
  manifest: QcNativeManifest | null;
  packageRoot: string;
}

export function resolveQcNativeArtifact(options: {
  packageRoot?: string;
  env?: NodeJS.ProcessEnv;
  platform?: NodeJS.Platform;
  arch?: string;
} = {}): QcNativeArtifact {
  const env = options.env ?? process.env;
  const packageRoot = resolve(options.packageRoot ?? packageRootFromModule());

  // Explicit absolute override exists only for development/test/rehearsal. It
  // is never a PATH lookup and requires a second opt-in flag so production
  // cannot silently bind an unrelated executable.
  const override = env.QUIET_CONTEXT_NATIVE_BIN;
  if (override) {
    if (env.QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE !== "1" || !isAbsolute(override)) {
      throw new QcNativeError(
        "QUIET_CONTEXT_NATIVE_BIN requires an absolute path and QUIET_CONTEXT_NATIVE_ALLOW_OVERRIDE=1",
        "missing-artifact",
      );
    }
    const binaryPath = resolve(override);
    if (!existsSync(binaryPath)) {
      throw new QcNativeError(`native override does not exist: ${binaryPath}`, "missing-artifact");
    }
    return { binaryPath, manifest: null, packageRoot };
  }

  const platformName = options.platform ?? process.platform;
  const archName = options.arch ?? process.arch;
  const key = qcNativePlatformKey(platformName, archName);
  if (!key) {
    throw new QcNativeError(
      `QuietContext native engine is not packaged for ${platformName}-${archName}`,
      "unsupported-platform",
    );
  }
  const base = resolve(packageRoot, "vendor", "qc-native", key);
  const binaryPath = resolve(base, platformName === "win32" ? "qc-native.exe" : "qc-native");
  const manifestPath = resolve(base, "manifest.json");
  if (!existsSync(binaryPath) || !existsSync(manifestPath)) {
    throw new QcNativeError(`native artifact is missing for ${key}`, "missing-artifact");
  }
  const manifest = validateManifest(parseJsonObject(readFileSync(manifestPath, "utf8")), manifestPath);
  const packageVersion = readPackageVersion(packageRoot);
  if (manifest.packageVersion !== packageVersion) {
    throw new QcNativeError(
      `native package version ${manifest.packageVersion} does not match QuietContext ${packageVersion}`,
      "package-version-mismatch",
    );
  }
  if (manifest.protocolVersion !== QC_NATIVE_PROTOCOL_VERSION) {
    throw new QcNativeError(
      `native protocol ${manifest.protocolVersion} does not match expected ${QC_NATIVE_PROTOCOL_VERSION}`,
      "protocol-mismatch",
    );
  }
  if (manifest.platform !== platformName || manifest.arch !== archName) {
    throw new QcNativeError(
      `native manifest target ${manifest.platform}-${manifest.arch} does not match ${platformName}-${archName}`,
      "invalid-manifest",
    );
  }
  const actual = createHash("sha256").update(readFileSync(binaryPath)).digest("hex");
  if (actual !== manifest.sha256) {
    throw new QcNativeError(`native artifact hash mismatch: ${binaryPath}`, "hash-mismatch");
  }
  return { binaryPath, manifest, packageRoot };
}

function killProcessTree(child: import("node:child_process").ChildProcess): void {
  if (!child.pid) return;
  try {
    if (process.platform === "win32") {
      // No shell expansion; taskkill receives the pid as an argv token.
      spawn("taskkill", ["/pid", String(child.pid), "/t", "/f"], { stdio: "ignore" }).unref();
    } else {
      process.kill(-child.pid, "SIGTERM");
      setTimeout(() => {
        try { process.kill(-child.pid!, "SIGKILL"); } catch { /* already gone */ }
      }, 500).unref();
    }
  } catch {
    try { child.kill("SIGKILL"); } catch { /* already gone */ }
  }
}

async function invokeNativeJson(
  args: string[],
  options: { cwd?: string; timeoutMs?: number; packageRoot?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<Record<string, unknown>> {
  const artifact = resolveQcNativeArtifact({ packageRoot: options.packageRoot, env: options.env });
  const timeoutMs = options.timeoutMs;
  if (timeoutMs !== undefined && (!Number.isFinite(timeoutMs) || timeoutMs < 1 || timeoutMs > 24 * 60 * 60 * 1000)) {
    throw new QcNativeError(`invalid native timeout: ${timeoutMs}`, "timeout");
  }
  return await new Promise((resolvePromise, rejectPromise) => {
    let stdoutBytes = 0;
    let stderrBytes = 0;
    const stdout: Buffer[] = [];
    const stderr: Buffer[] = [];
    let oversized = false;
    let timedOut = false;
    const child = spawn(artifact.binaryPath, args, {
      cwd: options.cwd,
      env: options.env ?? process.env,
      shell: false,
      detached: process.platform !== "win32",
      stdio: ["ignore", "pipe", "pipe"],
    });
    const timer = timeoutMs === undefined ? undefined : setTimeout(() => {
      timedOut = true;
      killProcessTree(child);
    }, timeoutMs);
    timer?.unref();

    child.on("error", (error) => {
      if (timer) clearTimeout(timer);
      rejectPromise(new QcNativeError(`failed to start native engine: ${error.message}`, "spawn-failed"));
    });
    child.stdout?.on("data", (chunk: Buffer) => {
      stdoutBytes += chunk.length;
      if (stdoutBytes > MAX_NATIVE_STDOUT_BYTES) {
        oversized = true;
        killProcessTree(child);
        return;
      }
      stdout.push(Buffer.from(chunk));
    });
    child.stderr?.on("data", (chunk: Buffer) => {
      stderrBytes += chunk.length;
      if (stderrBytes <= MAX_NATIVE_STDERR_BYTES) stderr.push(Buffer.from(chunk));
    });
    child.on("close", (code, signal) => {
      if (timer) clearTimeout(timer);
      if (timedOut) {
        rejectPromise(new QcNativeError(`native engine timed out after ${timeoutMs ?? "unknown"}ms`, "timeout"));
        return;
      }
      if (oversized) {
        rejectPromise(new QcNativeError("native engine exceeded its response budget", "oversized-response"));
        return;
      }
      if (code !== 0) {
        const detail = Buffer.concat(stderr).toString("utf8").trim();
        rejectPromise(new QcNativeError(
          `native engine exited ${code ?? signal ?? "unknown"}${detail ? `: ${detail}` : ""}`,
          "invalid-response",
        ));
        return;
      }
      try {
        resolvePromise(parseJsonObject(Buffer.concat(stdout).toString("utf8").trim()));
      } catch (error) {
        rejectPromise(error);
      }
    });
  });
}

function assertBaseReceipt(value: Record<string, unknown>, kind: "run" | "repo"): void {
  if (value.protocolVersion !== QC_NATIVE_PROTOCOL_VERSION || value.kind !== kind) {
    throw new QcNativeError(`invalid ${kind} receipt protocol/kind`, "invalid-response");
  }
  if (typeof value.nativeVersion !== "string") {
    throw new QcNativeError(`invalid ${kind} receipt nativeVersion`, "invalid-response");
  }
}

export async function verifyQcNative(options: {
  packageRoot?: string;
  env?: NodeJS.ProcessEnv;
  timeoutMs?: number;
} = {}): Promise<NativeStatus> {
  const artifact = resolveQcNativeArtifact({ packageRoot: options.packageRoot, env: options.env });
  const value = await invokeNativeJson(["status"], { ...options, timeoutMs: options.timeoutMs ?? 5_000 });
  if (
    value.protocolVersion !== QC_NATIVE_PROTOCOL_VERSION ||
    typeof value.nativeVersion !== "string" ||
    value.product !== "QuietContext"
  ) {
    throw new QcNativeError("native status handshake is incompatible", "protocol-mismatch");
  }
  if (artifact.manifest && value.nativeVersion !== artifact.manifest.nativeVersion) {
    throw new QcNativeError(
      `native runtime version ${value.nativeVersion} does not match manifest ${artifact.manifest.nativeVersion}`,
      "native-version-mismatch",
    );
  }
  return value as unknown as NativeStatus;
}

export async function runQcNative(
  argv: string[],
  options: { cwd?: string; timeoutMs?: number; packageRoot?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<QcNativeRunReceipt> {
  if (argv.length === 0 || argv.some((arg) => typeof arg !== "string" || arg.includes("\0"))) {
    throw new QcNativeError("native run requires non-empty NUL-free argv", "invalid-response");
  }
  const value = await invokeNativeJson(["run", "--", ...argv], options);
  assertBaseReceipt(value, "run");
  if (
    !Array.isArray(value.command) ||
    typeof value.exitCode !== "number" ||
    !value.stdout || typeof value.stdout !== "object" ||
    !value.stderr || typeof value.stderr !== "object"
  ) {
    throw new QcNativeError("invalid native run receipt", "invalid-response");
  }
  return value as unknown as QcNativeRunReceipt;
}

export async function repoQcNative(
  request:
    | { action: "map"; root?: string }
    | { action: "symbol" | "references"; query: string; root?: string }
    | { action: "outline"; path: string; root?: string },
  options: { cwd?: string; timeoutMs?: number; packageRoot?: string; env?: NodeJS.ProcessEnv } = {},
): Promise<QcNativeRepoReceipt> {
  const args = ["repo", request.action];
  if (request.action === "symbol" || request.action === "references") args.push(request.query);
  if (request.action === "outline") args.push(request.path);
  if (request.root) args.push("--root", request.root);
  const value = await invokeNativeJson(args, options);
  assertBaseReceipt(value, "repo");
  if (
    typeof value.operation !== "string" ||
    typeof value.root !== "string" ||
    typeof value.stdout !== "string" ||
    typeof value.stderr !== "string" ||
    typeof value.exitCode !== "number"
  ) {
    throw new QcNativeError("invalid native repo receipt", "invalid-response");
  }
  return value as unknown as QcNativeRepoReceipt;
}
