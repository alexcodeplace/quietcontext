import { mkdirSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { afterEach, describe, expect, test } from "vitest";
import { ContentStore } from "../src/store.js";
import { archiveNativeRunEvidence, nativeRunNeedsEvidence, pruneNativeEvidence } from "../src/native-evidence.js";
import type { QcNativeRunReceipt, QcNativeStreamReceipt } from "../src/native-qc.js";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function stream(path: string, content: Buffer, overrides: Partial<QcNativeStreamReceipt> = {}): QcNativeStreamReceipt {
  writeFileSync(path, content);
  return {
    compact: content.toString("utf8"),
    rawPath: path,
    rawBytes: content.length,
    rawComplete: true,
    sampleComplete: true,
    compactBytes: content.length,
    capped: false,
    ...overrides,
  };
}

function receipt(root: string, stdout: QcNativeStreamReceipt, stderr: QcNativeStreamReceipt): QcNativeRunReceipt {
  return {
    protocolVersion: 1,
    nativeVersion: "0.1.0",
    kind: "run",
    command: ["cargo", "test"],
    exitCode: 1,
    filter: "cargo",
    filterInputBytes: stdout.rawBytes,
    filtered: true,
    deduped: false,
    stdout,
    stderr,
  };
}

describe("qc native evidence archival", () => {
  test("archives exact raw bytes and makes omitted text searchable", () => {
    const root = mkdtempSync(join(tmpdir(), "qc-evidence-")); roots.push(root);
    const spool = join(root, "spool"); mkdirSync(spool);
    const evidence = join(root, "evidence");
    const db = join(root, "content.db");
    const store = new ContentStore(db);
    const raw = Buffer.from("line one\nUNIQUE_CANARY_7b9d2\nline three\n");
    const out = stream(join(spool, "stdout.raw"), raw, {
      compact: "line one\nline three\n",
      compactBytes: 20,
      capped: true,
    });
    const err = stream(join(spool, "stderr.raw"), Buffer.alloc(0));

    const archived = archiveNativeRunEvidence(receipt(root, out, err), {
      store,
      evidenceRoot: evidence,
      projectId: "project-1",
      now: new Date("2026-09-09T12:00:00Z"),
    });
    expect(archived.archived).toBe(true);
    expect(archived.searchable).toBe(true);
    expect(archived.sources).toHaveLength(1);
    const stdout = archived.streams.find((s) => s.stream === "stdout")!;
    expect(readFileSync(stdout.evidencePath!)).toEqual(raw);
    expect(stdout.sha256).toMatch(/^[a-f0-9]{64}$/);
    expect(store.searchWithFallback("UNIQUE_CANARY_7b9d2", 5, stdout.source)).toHaveLength(1);
    expect(archived.note).toContain("search recovers omitted raw output");
    store.cleanup();
  });

  test("incomplete native spool never claims recoverability", () => {
    const root = mkdtempSync(join(tmpdir(), "qc-evidence-")); roots.push(root);
    const spool = join(root, "spool"); mkdirSync(spool);
    const store = new ContentStore(join(root, "content.db"));
    const raw = Buffer.from("partial");
    const out = stream(join(spool, "stdout.raw"), raw, {
      rawBytes: 10000,
      rawComplete: false,
      capped: true,
    });
    const err = stream(join(spool, "stderr.raw"), Buffer.alloc(0));
    const archived = archiveNativeRunEvidence(receipt(root, out, err), {
      store,
      evidenceRoot: join(root, "evidence"),
      projectId: "project-1",
    });
    expect(archived.archived).toBe(false);
    expect(archived.searchable).toBe(false);
    expect(archived.note).toContain("evidence unavailable");
    store.cleanup();
  });

  test("lossless small commands discard transient spools instead of retaining noise", () => {
    const root = mkdtempSync(join(tmpdir(), "qc-evidence-")); roots.push(root);
    const spool = join(root, "spool"); mkdirSync(spool);
    const store = new ContentStore(join(root, "content.db"));
    const out = stream(join(spool, "stdout.raw"), Buffer.from("ok\n"));
    const err = stream(join(spool, "stderr.raw"), Buffer.alloc(0));
    const r = receipt(root, out, err);
    r.filtered = false;
    expect(nativeRunNeedsEvidence(r)).toBe(false);
    const archived = archiveNativeRunEvidence(r, {
      store,
      evidenceRoot: join(root, "evidence"),
      projectId: "project-1",
    });
    expect(archived.archived).toBe(false);
    expect(() => readFileSync(out.rawPath)).toThrow();
    store.cleanup();
  });

  test("retention removes stale runs and enforces a byte ceiling oldest-first", () => {
    const root = mkdtempSync(join(tmpdir(), "qc-evidence-prune-")); roots.push(root);
    const old = join(root, "old"); const newer = join(root, "newer"); const newest = join(root, "newest");
    for (const dir of [old, newer, newest]) { mkdirSync(dir); writeFileSync(join(dir, "stdout.raw"), Buffer.alloc(10)); }
    const now = Date.now();
    // Directory mtimes follow the contained file by default closely enough; force old via utimes.
    utimesSync(old, new Date(now - 10_000), new Date(now - 10_000));
    utimesSync(newer, new Date(now - 2_000), new Date(now - 2_000));
    utimesSync(newest, new Date(now - 1_000), new Date(now - 1_000));
    const pruned = pruneNativeEvidence(root, { nowMs: now, maxAgeMs: 5_000, maxBytes: 10 });
    expect(pruned.removed).toBe(2);
    expect(pruned.retainedBytes).toBe(10);
    expect(() => readFileSync(join(newest, "stdout.raw"))).not.toThrow();
  });
});
