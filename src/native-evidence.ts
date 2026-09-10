import { createHash, randomBytes } from "node:crypto";
import { isUtf8 } from "node:buffer";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  lstatSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { basename, join, resolve } from "node:path";
import type { ContentStore } from "./store.js";
import { MAX_SOURCE_BYTES } from "./store.js";
import type { QcNativeRunReceipt, QcNativeStreamReceipt } from "./native-qc.js";

const DEFAULT_EVIDENCE_MAX_AGE_MS = 14 * 24 * 60 * 60 * 1000;
const DEFAULT_EVIDENCE_MAX_BYTES = 128 * 1024 * 1024;

export interface NativeEvidenceStreamResult {
  stream: "stdout" | "stderr";
  rawBytes: number;
  rawComplete: boolean;
  archived: boolean;
  indexed: boolean;
  evidencePath?: string;
  source?: string;
  sha256?: string;
  reason?: string;
}

export interface NativeEvidenceReceipt {
  evidenceId: string;
  archived: boolean;
  searchable: boolean;
  streams: NativeEvidenceStreamResult[];
  sources: string[];
  note: string;
}

export interface NativeEvidenceOptions {
  store: ContentStore;
  evidenceRoot: string;
  projectId: string;
  attribution?: { sessionId?: string; eventId?: string };
  now?: Date;
  maxAgeMs?: number;
  maxEvidenceBytes?: number;
}

function streamIsLossy(stream: QcNativeStreamReceipt): boolean {
  return stream.capped || !stream.sampleComplete || stream.rawBytes > stream.compactBytes;
}

export function nativeRunNeedsEvidence(receipt: QcNativeRunReceipt): boolean {
  return receipt.filtered
    || receipt.deduped
    || streamIsLossy(receipt.stdout)
    || streamIsLossy(receipt.stderr);
}

function safeCommandLabel(command: string[]): string {
  const first = basename(command[0] || "command").replace(/[^A-Za-z0-9._-]/g, "_");
  return first.slice(0, 32) || "command";
}

function dirSize(path: string): number {
  let total = 0;
  for (const entry of readdirSync(path, { withFileTypes: true })) {
    const child = join(path, entry.name);
    try {
      if (entry.isSymbolicLink()) continue;
      if (entry.isDirectory()) total += dirSize(child);
      else if (entry.isFile()) total += statSync(child).size;
    } catch { /* concurrent cleanup; ignore */ }
  }
  return total;
}

export function pruneNativeEvidence(
  evidenceRoot: string,
  options: { nowMs?: number; maxAgeMs?: number; maxBytes?: number } = {},
): { removed: number; retainedBytes: number } {
  if (!existsSync(evidenceRoot)) return { removed: 0, retainedBytes: 0 };
  const nowMs = options.nowMs ?? Date.now();
  const maxAgeMs = options.maxAgeMs ?? DEFAULT_EVIDENCE_MAX_AGE_MS;
  const maxBytes = options.maxBytes ?? DEFAULT_EVIDENCE_MAX_BYTES;
  const dirs: Array<{ path: string; mtimeMs: number; bytes: number }> = [];
  let removed = 0;

  for (const entry of readdirSync(evidenceRoot, { withFileTypes: true })) {
    const child = join(evidenceRoot, entry.name);
    try {
      if (!entry.isDirectory() || entry.isSymbolicLink()) continue;
      const st = lstatSync(child);
      if (nowMs - st.mtimeMs > maxAgeMs) {
        rmSync(child, { recursive: true, force: true });
        removed++;
        continue;
      }
      dirs.push({ path: child, mtimeMs: st.mtimeMs, bytes: dirSize(child) });
    } catch { /* best effort */ }
  }

  let retainedBytes = dirs.reduce((sum, item) => sum + item.bytes, 0);
  dirs.sort((a, b) => a.mtimeMs - b.mtimeMs);
  for (const item of dirs) {
    if (retainedBytes <= maxBytes) break;
    try {
      rmSync(item.path, { recursive: true, force: true });
      retainedBytes = Math.max(0, retainedBytes - item.bytes);
      removed++;
    } catch { /* best effort */ }
  }
  return { removed, retainedBytes };
}

function moveExact(source: string, destination: string): void {
  try {
    renameSync(source, destination);
  } catch {
    copyFileSync(source, destination);
    unlinkSync(source);
  }
  chmodSync(destination, 0o600);
}

function discardSpool(path: string): void {
  try { unlinkSync(path); } catch { /* best effort */ }
}

function archiveStream(
  receipt: QcNativeRunReceipt,
  streamName: "stdout" | "stderr",
  stream: QcNativeStreamReceipt,
  runDir: string,
  evidenceId: string,
  options: NativeEvidenceOptions,
): NativeEvidenceStreamResult {
  const result: NativeEvidenceStreamResult = {
    stream: streamName,
    rawBytes: stream.rawBytes,
    rawComplete: stream.rawComplete,
    archived: false,
    indexed: false,
  };
  if (stream.rawBytes === 0) {
    discardSpool(stream.rawPath);
    result.reason = "empty";
    return result;
  }
  if (!stream.rawComplete) {
    discardSpool(stream.rawPath);
    result.reason = "native-spool-incomplete";
    return result;
  }
  if (stream.rawBytes > MAX_SOURCE_BYTES) {
    discardSpool(stream.rawPath);
    result.reason = `raw-source-exceeds-${MAX_SOURCE_BYTES}-byte-index-limit`;
    return result;
  }

  try {
    const canonical = resolve(stream.rawPath);
    const st = lstatSync(canonical);
    if (!st.isFile() || st.isSymbolicLink() || st.size !== stream.rawBytes) {
      discardSpool(canonical);
      result.reason = "spool-integrity-mismatch";
      return result;
    }
    const destination = join(runDir, `${streamName}.raw`);
    moveExact(canonical, destination);
    const raw = readFileSync(destination);
    const sha256 = createHash("sha256").update(raw).digest("hex");
    result.archived = true;
    result.evidencePath = destination;
    result.sha256 = sha256;

    if (!isUtf8(raw)) {
      result.reason = "binary-or-invalid-utf8-not-search-indexed";
      return result;
    }
    const source = `command-output:${evidenceId}:${streamName}:${safeCommandLabel(receipt.command)}`;
    options.store.index({
      content: raw.toString("utf8"),
      source,
      attribution: options.attribution,
      sourceCategory: "command-output",
      contentHash: sha256,
    });
    result.indexed = true;
    result.source = source;
    return result;
  } catch (error) {
    discardSpool(stream.rawPath);
    result.reason = `archive-or-index-failed:${error instanceof Error ? error.message : String(error)}`;
    return result;
  }
}

export function archiveNativeRunEvidence(
  receipt: QcNativeRunReceipt,
  options: NativeEvidenceOptions,
): NativeEvidenceReceipt {
  const now = options.now ?? new Date();
  const evidenceId = `${now.toISOString().replace(/[-:.TZ]/g, "").slice(0, 14)}-${randomBytes(5).toString("hex")}`;
  if (!nativeRunNeedsEvidence(receipt)) {
    discardSpool(receipt.stdout.rawPath);
    discardSpool(receipt.stderr.rawPath);
    return {
      evidenceId,
      archived: false,
      searchable: false,
      streams: [],
      sources: [],
      note: "",
    };
  }

  mkdirSync(options.evidenceRoot, { recursive: true, mode: 0o700 });
  chmodSync(options.evidenceRoot, 0o700);
  pruneNativeEvidence(options.evidenceRoot, {
    nowMs: now.getTime(),
    maxAgeMs: options.maxAgeMs,
    maxBytes: options.maxEvidenceBytes,
  });

  const runDir = join(options.evidenceRoot, evidenceId);
  mkdirSync(runDir, { recursive: false, mode: 0o700 });
  const streams = [
    archiveStream(receipt, "stdout", receipt.stdout, runDir, evidenceId, options),
    archiveStream(receipt, "stderr", receipt.stderr, runDir, evidenceId, options),
  ];
  const sources = streams.flatMap((stream) => stream.indexed && stream.source ? [stream.source] : []);
  const archived = streams.some((stream) => stream.archived);
  const searchable = sources.length > 0;

  const metadata = {
    schemaVersion: 1,
    evidenceId,
    createdAt: now.toISOString(),
    projectId: options.projectId,
    command: receipt.command,
    exitCode: receipt.exitCode,
    nativeVersion: receipt.nativeVersion,
    protocolVersion: receipt.protocolVersion,
    filtered: receipt.filtered,
    deduped: receipt.deduped,
    streams: streams.map(({ evidencePath, ...stream }) => ({
      ...stream,
      file: evidencePath ? basename(evidencePath) : undefined,
    })),
  };
  writeFileSync(join(runDir, "metadata.json"), JSON.stringify(metadata, null, 2) + "\n", { mode: 0o600 });

  if (!archived) {
    try { rmSync(runDir, { recursive: true, force: true }); } catch { /* best effort */ }
  }

  const note = searchable
    ? `\n[qc evidence: ${sources.map((source) => `source="${source}"`).join(", ")} — search recovers omitted raw output]`
    : archived
      ? `\n[qc evidence: ${evidenceId} archived exactly but is not search-indexed]`
      : "\n[qc evidence unavailable: raw spool was incomplete or ingestion failed]";
  return { evidenceId, archived, searchable, streams, sources, note };
}
