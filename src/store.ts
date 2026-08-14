/**
 * ContentStore — FTS5 BM25-based knowledge base for context-mode.
 *
 * Chunks markdown content by headings (keeping code blocks intact),
 * stores in SQLite FTS5, and retrieves via BM25-ranked search.
 *
 * Use for documentation, API references, and any content where
 * you need EXACT text later — not summaries.
 */

import type { Database as DatabaseInstance } from "better-sqlite3";
import { loadDatabase, applyWALPragmas, withRetry } from "./db-base.js";
import type { PreparedStatement } from "./db-base.js";
import { readFileSync, readdirSync, unlinkSync, existsSync, statSync, openSync, fstatSync, closeSync } from "node:fs";
import { createHash, randomBytes } from "node:crypto";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { walkDirectoryDetailed, type WalkOptions } from "./store-directory.js";
import {
  acquireContentStoreMigrationFence,
  acquireContentStoreOwnerFence,
} from "./content-store-fence.js";

// ─────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────

interface Chunk {
  title: string;
  content: string;
  hasCode: boolean;
}

type SourceMatchMode = "like" | "exact";

type SearchRow = {
  title: string;
  content: string;
  content_type: string;
  timestamp: string | null;
  label: string;
  rank: number;
  highlighted: string;
  /** Attribution session_id (empty string for legacy unattributed chunks). */
  session_id: string;
};

import type { IndexResult, SearchResult, StoreStats } from "./types.js";
export type { IndexResult, SearchResult, StoreStats } from "./types.js";

// ─────────────────────────────────────────────────────────
// Constants
// ─────────────────────────────────────────────────────────

const STOPWORDS = new Set([
  "the", "and", "for", "are", "but", "not", "you", "all", "can", "had",
  "her", "was", "one", "our", "out", "has", "his", "how", "its", "may",
  "new", "now", "old", "see", "way", "who", "did", "get", "got", "let",
  "say", "she", "too", "use", "will", "with", "this", "that", "from",
  "they", "been", "have", "many", "some", "them", "than", "each", "make",
  "like", "just", "over", "such", "take", "into", "year", "your", "good",
  "could", "would", "about", "which", "their", "there", "other", "after",
  "should", "through", "also", "more", "most", "only", "very", "when",
  "what", "then", "these", "those", "being", "does", "done", "both",
  "same", "still", "while", "where", "here", "were", "much",
  // Common in code/changelogs
  "update", "updates", "updated", "deps", "dev", "tests", "test",
  "add", "added", "fix", "fixed", "run", "running", "using",
]);

// ─────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────

/**
 * Remove case-insensitive duplicate tokens while preserving the first
 * occurrence's original casing. FTS5's unicode61 tokenizer lowercases on
 * both sides, so `"Error" OR "error"` produces no extra recall — just
 * redundant index lookups. Dedup keeps the compiled query minimal.
 */
function dedupeTokens(tokens: string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const t of tokens) {
    const key = t.toLowerCase();
    if (!seen.has(key)) {
      seen.add(key);
      out.push(t);
    }
  }
  return out;
}

export function sanitizeQuery(query: string, mode: "AND" | "OR" = "AND"): string {
  const words = dedupeTokens(
    query
      .replace(/['"(){}[\]*:^~]/g, " ")
      .split(/\s+/)
      .filter(
        (w) =>
          w.length > 0 &&
          !["AND", "OR", "NOT", "NEAR"].includes(w.toUpperCase()),
      ),
  );

  if (words.length === 0) return '""';

  // Filter stopwords to improve BM25 ranking — common terms like "update",
  // "test", "fix" appear everywhere and dilute relevance scoring.
  // Fall back to unfiltered words if ALL terms are stopwords.
  const meaningful = words.filter((w) => !STOPWORDS.has(w.toLowerCase()));
  const final = meaningful.length > 0 ? meaningful : words;

  return final.map((w) => `"${w}"`).join(mode === "OR" ? " OR " : " ");
}

export function sanitizeTrigramQuery(query: string, mode: "AND" | "OR" = "AND"): string {
  const cleaned = query.replace(/["'(){}[\]*:^~]/g, "").trim();
  if (cleaned.length < 3) return "";
  const words = dedupeTokens(
    cleaned.split(/\s+/).filter((w) => w.length >= 3),
  );
  if (words.length === 0) return "";

  const meaningful = words.filter((w) => !STOPWORDS.has(w.toLowerCase()));
  const final = meaningful.length > 0 ? meaningful : words;

  return final.map((w) => `"${w}"`).join(mode === "OR" ? " OR " : " ");
}

function levenshtein(a: string, b: string): number {
  if (a.length === 0) return b.length;
  if (b.length === 0) return a.length;
  let prev = Array.from({ length: b.length + 1 }, (_, i) => i);
  for (let i = 1; i <= a.length; i++) {
    const curr = [i];
    for (let j = 1; j <= b.length; j++) {
      curr[j] =
        a[i - 1] === b[j - 1]
          ? prev[j - 1]
          : 1 + Math.min(prev[j], curr[j - 1], prev[j - 1]);
    }
    prev = curr;
  }
  return prev[b.length];
}

function maxEditDistance(wordLength: number): number {
  if (wordLength <= 4) return 1;
  if (wordLength <= 12) return 2;
  return 3;
}

function vocabularyGrams(word: string): string[] {
  const padded = `^${word}$`;
  const grams = new Set<string>();
  for (let index = 0; index <= padded.length - 3; index++) {
    grams.add(padded.slice(index, index + 3));
  }
  return [...grams];
}

// Oversized chunks (e.g., a 50KB section between two headings) hurt BM25
// length normalization and produce unwieldy search results. Split at paragraph
// boundaries when a chunk exceeds this cap.
const MAX_CHUNK_BYTES = 4096;

// Blank-line sectioning is used only for output that is *naturally* sectioned:
// at least a few sections, not an unbounded explosion, and no single section so
// large that the split is clearly not the real structure (those fall back to
// line-grouping). Sections that pass the heuristic but still exceed
// MAX_CHUNK_BYTES are sub-split so no persisted chunk breaks the cap.
const MIN_BLANK_LINE_SECTIONS = 3;
const MAX_BLANK_LINE_SECTIONS = 200;
const BLANK_SECTION_STRATEGY_MAX_BYTES = 5000;

// Number of leading characters of a chunk's first line used as its title.
const CHUNK_TITLE_MAX_CHARS = 80;

// When byte-splitting an oversized single line, prefer to break at a whitespace
// boundary for readability — but only if that boundary is past this fraction of
// the slice, otherwise we'd waste too much of the byte budget.
const WHITESPACE_BREAK_RATIO = 0.5;

export const MAX_INDEX_INPUT_BYTES = 32 * 1024 * 1024;
export const MAX_SOURCE_LABEL_BYTES = 4 * 1024;
export const MAX_INDEX_LINES = 250_000;
export const MAX_JSON_DEPTH = 64;
export const MAX_JSON_NODES = 100_000;

export const CONTENT_STORE_LIMITS = Object.freeze({
  maxIndexedBytes: 32 * 1024 * 1024,
  maxChunks: 12_000,
  maxSources: 1_000,
  maxVocabularyTerms: 50_000,
  maxFuzzyCandidates: 256,
  maxFuzzyWordLength: 64,
  maxFuzzyQueryWords: 8,
  maxMetadataChecks: 256,
  maxDistinctiveChunks: 512,
  maintenanceChunkBatch: 256,
  walCheckpointBytes: 8 * 1024 * 1024,
});

export interface ContentStoreLimits {
  maxIndexedBytes: number;
  maxChunks: number;
  maxSources: number;
  maxVocabularyTerms: number;
  maxFuzzyCandidates: number;
  maxFuzzyWordLength: number;
  maxFuzzyQueryWords: number;
  maxMetadataChecks: number;
  maxDistinctiveChunks: number;
  maintenanceChunkBatch: number;
  walCheckpointBytes: number;
}

// ─────────────────────────────────────────────────────────
// ContentStore
// ─────────────────────────────────────────────────────────

/**
 * Remove stale DB files from previous sessions whose processes no longer exist.
 */
export function cleanupStaleDBs(): number {
  const dir = tmpdir();
  let cleaned = 0;
  try {
    const files = readdirSync(dir);
    for (const file of files) {
      const match = file.match(/^context-mode-(\d+)\.db$/);
      if (!match) continue;
      const pid = parseInt(match[1], 10);
      if (pid === process.pid) continue;
      try {
        process.kill(pid, 0);
      } catch {
        const base = join(dir, file);
        for (const suffix of ["", "-wal", "-shm"]) {
          try { unlinkSync(base + suffix); } catch { /* ignore */ }
        }
        cleaned++;
      }
    }
  } catch { /* ignore readdir errors */ }
  return cleaned;
}

/**
 * Check if a PID is still alive (not a zombie holding a WAL lock).
 * Returns true if the process exists, false if it's dead.
 */
function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/**
 * Clean up stale per-project content store DBs older than maxAgeDays.
 * Scans the given directory for *.db files and checks mtime.
 * Also detects zombie processes holding WAL locks — if a WAL file exists
 * but the owning PID is dead, the DB files are cleaned up regardless of age.
 */
export function cleanupStaleContentDBs(contentDir: string, maxAgeDays: number): number {
  let cleaned = 0;
  try {
    if (!existsSync(contentDir)) return 0;
    const cutoff = Date.now() - maxAgeDays * 24 * 60 * 60 * 1000;
    const files = readdirSync(contentDir).filter(f => f.endsWith(".db"));
    for (const file of files) {
      try {
        const filePath = join(contentDir, file);
        const mtime = statSync(filePath).mtimeMs;
        let shouldClean = mtime < cutoff;

        // Detect zombie processes holding WAL locks:
        // If a WAL file exists, try to read the WAL header to extract the PID.
        // WAL files from dead processes can block new connections.
        if (!shouldClean) {
          const walPath = filePath + "-wal";
          if (existsSync(walPath)) {
            try {
              const walStat = statSync(walPath);
              // If WAL file is non-empty and DB hasn't been modified in >1 hour,
              // the owning process may be dead — check via mtime staleness
              if (walStat.size > 0 && (Date.now() - walStat.mtimeMs) > 3600_000) {
                shouldClean = true;
              }
            } catch { /* ignore WAL check errors */ }
          }
        }

        if (shouldClean) {
          for (const suffix of ["", "-wal", "-shm"]) {
            try { unlinkSync(filePath + suffix); } catch { /* ignore */ }
          }
          cleaned++;
        }
      } catch { /* ignore per-file errors */ }
    }
  } catch { /* ignore readdir errors */ }
  return cleaned;
}

// ── Proximity helpers (pure functions) ──

/** Find all positions of a term in text. */
function findAllPositions(text: string, term: string): number[] {
  const positions: number[] = [];
  let idx = text.indexOf(term);
  while (idx !== -1) {
    positions.push(idx);
    idx = text.indexOf(term, idx + 1);
  }
  return positions;
}

/**
 * Count matched adjacent pairs across consecutive query terms.
 * For each pair (term[i], term[i+1]), pairs each left position with at most one
 * right position whose offset falls within `gap` chars of `p + len(term[i])`.
 * `positionLists` must be sorted ascending (output of `findAllPositions` is).
 * Each right position is consumed by at most one left, so `"foo foo bar"`
 * counts 1 pair, not 2 — matches IR phrase-occurrence intent and avoids
 * inflating boosts for repeated-token queries.
 * Used by reranker to layer a frequency signal on top of minSpan proximity:
 * 30-char gap covers natural prose without rewarding distant matches.
 */
function countAdjacentPairs(
  positionLists: number[][],
  terms: string[],
  gap: number = 30,
): number {
  if (positionLists.length < 2 || terms.length < 2) return 0;
  let total = 0;
  const pairs = Math.min(positionLists.length, terms.length) - 1;
  for (let i = 0; i < pairs; i++) {
    const left = positionLists[i];
    const right = positionLists[i + 1];
    const leftLen = terms[i].length;
    let j = 0;
    for (const p of left) {
      const minStart = p + leftLen;
      const maxStart = minStart + gap;
      while (j < right.length && right[j] < minStart) j++;
      if (j < right.length && right[j] <= maxStart) {
        total++;
        j++;
      }
    }
  }
  return total;
}

/**
 * Find minimum span (window) covering at least one position from each list.
 * Uses a sweep-line approach: advance the pointer at the current minimum.
 */
function findMinSpan(positionLists: number[][]): number {
  if (positionLists.length === 0) return Infinity;
  if (positionLists.length === 1) return 0;

  const sorted = positionLists;
  const ptrs = new Array(sorted.length).fill(0);
  let minSpan = Infinity;

  while (true) {
    let curMin = Infinity;
    let curMax = -Infinity;
    let minIdx = 0;

    for (let i = 0; i < sorted.length; i++) {
      const val = sorted[i][ptrs[i]];
      if (val < curMin) {
        curMin = val;
        minIdx = i;
      }
      if (val > curMax) {
        curMax = val;
      }
    }

    const span = curMax - curMin;
    if (span < minSpan) minSpan = span;

    ptrs[minIdx]++;
    if (ptrs[minIdx] >= sorted[minIdx].length) break;
  }

  return minSpan;
}

export function createProcessStorePath(): string {
  return join(tmpdir(), `context-mode-${process.pid}-${randomBytes(8).toString("hex")}.db`);
}

export class ContentStore {
  #db!: DatabaseInstance;
  #dbPath: string;
  #limits: ContentStoreLimits;
  #defaultBusyTimeoutMs: number;
  #ownerId = `${process.pid}-${Date.now()}-${Math.random().toString(36).slice(2)}`;
  #releaseOwnerFence: () => void = () => {};
  #closed = false;
  checkpointCount = 0;
  #writesSinceCheckpoint = 0;
  #nextCheckpointAt = 0;
  lastFuzzyCandidateCount = 0;
  lastMetadataCheckCount = 0;
  lastDistinctiveChunkCount = 0;
  lastMaintenanceBackfillRows = 0;
  vocabularyRebuildCount = 0;
  // Optional deny-policy callback. When set (by server.ts at startup),
  // #refreshStaleSources consults it before re-reading file_path during
  // auto-refresh. This catches policy edits between initial indexing and
  // a later search: a file that was allowed at index time may have been
  // added to the Read deny list afterwards. Without this hook, refresh
  // would re-read and re-expose the file. See #442 round-3.
  #denyChecker?: (filePath: string) => boolean;

  // ── Cached Prepared Statements ──
  // Prepared once at construction, reused on every call to avoid
  // re-compiling SQL on each invocation.

  // Write path
  #stmtInsertSourceEmpty!: PreparedStatement;
  #stmtInsertSource!: PreparedStatement;
  #stmtInsertChunk!: PreparedStatement;
  #stmtInsertChunkTrigram!: PreparedStatement;
  #stmtInsertVocab!: PreparedStatement;
  #stmtInsertVocabGram!: PreparedStatement;

  // Dedup path (delete previous source with same label before re-indexing)
  #stmtDeleteChunksByLabel!: PreparedStatement;
  #stmtDeleteChunksTrigramByLabel!: PreparedStatement;
  #stmtDeleteSourcesByLabel!: PreparedStatement;

  // Search path (hot)
  #stmtSearchPorter!: PreparedStatement;
  #stmtSearchPorterFiltered!: PreparedStatement;
  #stmtSearchPorterExact!: PreparedStatement;
  #stmtSearchTrigram!: PreparedStatement;
  #stmtSearchTrigramFiltered!: PreparedStatement;
  #stmtSearchTrigramExact!: PreparedStatement;
  #stmtSearchPorterContentType!: PreparedStatement;
  #stmtSearchPorterFilteredContentType!: PreparedStatement;
  #stmtSearchPorterExactContentType!: PreparedStatement;
  #stmtSearchTrigramContentType!: PreparedStatement;
  #stmtSearchTrigramFilteredContentType!: PreparedStatement;
  #stmtSearchTrigramExactContentType!: PreparedStatement;

  // Read path
  #stmtListSources!: PreparedStatement;
  #stmtChunksBySource!: PreparedStatement;
  #stmtSourceChunkCount!: PreparedStatement;
  #stmtChunkContent!: PreparedStatement;
  #stmtVocabularyContent!: PreparedStatement;
  #stmtStats!: PreparedStatement;
  #stmtSourceMeta!: PreparedStatement;

  // Cleanup path
  #stmtCleanupSource!: PreparedStatement;
  #stmtCleanupChunks!: PreparedStatement;
  #stmtCleanupChunksTrigram!: PreparedStatement;
  #stmtCleanupSourceIfEmpty!: PreparedStatement;

  // FTS5 optimization: track inserts and optimize periodically to defragment
  // the index. FTS5 b-trees fragment over many insert/delete cycles, degrading
  // search performance. SQLite's built-in 'optimize' merges b-tree segments.
  #insertCount = 0;
  static readonly OPTIMIZE_EVERY = 50;

  // Fuzzy correction cache (process-local LRU). fuzzyCorrect() hits the vocab
  // DB and runs levenshtein against every candidate within length tolerance,
  // which is CPU-linear in |candidates|. Repeated queries ("erro", "erro" …)
  // recompute the same answer. The vocabulary table is insert-only, so cache
  // entries only become stale when new words enter — we clear on actual insert.
  #fuzzyCache = new Map<string, string | null>();
  #fuzzyDataVersion = -1;
  static readonly FUZZY_CACHE_SIZE = 256;

  constructor(dbPath?: string, limits: Partial<ContentStoreLimits> = {}, timeoutMs: number = 30000) {
    const Database = loadDatabase();
    this.#dbPath = dbPath ?? createProcessStorePath();
    this.#limits = { ...CONTENT_STORE_LIMITS, ...limits };
    this.#defaultBusyTimeoutMs = timeoutMs;
    for (const [name, value] of Object.entries(this.#limits)) {
      if (!Number.isSafeInteger(value) || value <= 0) {
        throw new Error(`ContentStore limit ${name} must be a positive safe integer`);
      }
    }
    this.#releaseOwnerFence = acquireContentStoreOwnerFence(this.#dbPath);
    let db: DatabaseInstance | undefined;
    try {
      db = new Database(this.#dbPath, { timeout: timeoutMs });
      applyWALPragmas(db);
      this.#db = db;
      this.#initSchema();
      this.#db.prepare("INSERT INTO store_owners (owner_id, pid) VALUES (?, ?)").run(this.#ownerId, process.pid);
      this.#prepareStatements();
      this.#runMaintenanceBatch();
      this.#checkpointWALIfNeeded();
    } catch (err) {
      try { db?.close(); } catch { /* incomplete open */ }
      this.#releaseOwnerFence();
      const detail = err instanceof Error ? `: ${err.message}` : "";
      throw new Error(`Failed to open shared content store ${this.#dbPath}${detail}`, { cause: err });
    }
  }

  /** Delete this session's DB files when no shared peer remains. */
  cleanup(): void {
    try { this.#db.prepare("DELETE FROM store_owners WHERE owner_id = ?").run(this.#ownerId); } catch { /* closed */ }
    try { this.#db.close(); } catch { /* closed */ }
    this.#releaseOwnerFence();
    let releaseDeletionFence: (() => void) | undefined;
    try {
      releaseDeletionFence = acquireContentStoreMigrationFence(this.#dbPath);
      for (const suffix of ["", "-wal", "-shm"]) {
        try { unlinkSync(this.#dbPath + suffix); } catch { /* absent */ }
      }
    } catch { /* a shared peer still owns the store */ }
    finally { releaseDeletionFence?.(); }
  }

  #runBoundedWrite<T>(operation: () => T): T {
    this.#db.pragma(`busy_timeout = ${Math.min(50, this.#defaultBusyTimeoutMs)}`);
    try {
      return withRetry(operation, [10, 25, 50]);
    } finally {
      this.#db.pragma(`busy_timeout = ${this.#defaultBusyTimeoutMs}`);
    }
  }

  /** Remove one session's attributed chunks while preserving source accounting. */
  purgeSession(sessionId: string): number {
    if (!sessionId) throw new TypeError("ContentStore.purgeSession requires a sessionId");
    const purge = this.#db.transaction(() => {
      this.#db.prepare("UPDATE store_meta SET value = value + 1 WHERE key = 'purge_generation'").run();
      const row = this.#db.prepare(
        "SELECT COUNT(*) AS count FROM chunks WHERE session_id = ?",
      ).get(sessionId) as { count: number };
      if (row.count === 0) return 0;

      this.#db.exec("CREATE TEMP TABLE IF NOT EXISTS purge_source_ids (id INTEGER PRIMARY KEY); DELETE FROM purge_source_ids");
      this.#db.prepare(
        "INSERT INTO purge_source_ids SELECT DISTINCT source_id FROM chunks WHERE session_id = ?",
      ).run(sessionId);
      this.#db.prepare("DELETE FROM chunks WHERE session_id = ?").run(sessionId);
      this.#db.prepare("DELETE FROM chunks_trigram WHERE session_id = ?").run(sessionId);
      this.#db.exec(`
        UPDATE sources SET
          chunk_count = (SELECT COUNT(*) FROM chunks WHERE chunks.source_id = sources.id),
          code_chunk_count = (SELECT COUNT(*) FROM chunks WHERE chunks.source_id = sources.id AND chunks.content_type = 'code'),
          indexed_bytes = COALESCE((SELECT SUM(length(CAST(chunks.title AS BLOB)) + length(CAST(chunks.content AS BLOB))) FROM chunks WHERE chunks.source_id = sources.id), 0)
        WHERE id IN (SELECT id FROM purge_source_ids);
        DELETE FROM retention_queue WHERE source_id IN (SELECT id FROM purge_source_ids) AND source_id IN (SELECT id FROM sources WHERE chunk_count = 0);
        DELETE FROM sources WHERE id IN (SELECT id FROM purge_source_ids) AND chunk_count = 0;
        DELETE FROM purge_source_ids;
      `);
      this.#rebuildVocabulary();
      return row.count;
    });

    const removed = this.#runBoundedWrite(() => purge());
    if (removed > 0) {
      this.#fuzzyCache.clear();
      this.#writesSinceCheckpoint++;
      this.#checkpointWALIfNeeded();
    }
    return removed;
  }

  /** Clear indexed content without unlinking the shared SQLite database. */
  purgeAll(): boolean {
    const clear = this.#db.transaction(() => {
      const existed = Boolean(this.#db.prepare("SELECT 1 FROM sources LIMIT 1").get());
      this.#db.exec(`
        DELETE FROM chunks;
        DELETE FROM chunks_trigram;
        DELETE FROM retention_queue;
        DELETE FROM sources;
        DELETE FROM vocabulary_grams;
        DELETE FROM vocabulary;
        UPDATE store_totals SET indexed_bytes = 0, chunks = 0, sources = 0 WHERE singleton = 1;
        UPDATE store_meta SET value = CASE
          WHEN key = 'metadata_cursor' THEN 0
          WHEN key = 'purge_generation' THEN value + 1
          ELSE -1
        END;
      `);
      return existed;
    });
    const existed = this.#runBoundedWrite(() => clear());
    this.#fuzzyCache.clear();
    this.#writesSinceCheckpoint++;
    this.#checkpointWALIfNeeded();
    return existed;
  }

  // ── Schema ──

  #initSchema(): void {
    this.#db.pragma(`busy_timeout = ${Math.min(250, this.#defaultBusyTimeoutMs)}`);
    try {
      withRetry(() => {
        this.#db.exec("BEGIN IMMEDIATE");
        try {
          this.#initializeSchema();
          this.#db.exec("COMMIT");
        } catch (error) {
          try { this.#db.exec("ROLLBACK"); } catch { /* transaction did not start */ }
          throw error;
        }
      }, [10, 50, 200]);
    } finally {
      this.#db.pragma(`busy_timeout = ${this.#defaultBusyTimeoutMs}`);
    }
  }

  #initializeSchema(): void {
    const existingSources = Boolean(this.#db.prepare(
      "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sources'",
    ).get());
    const existingTotals = Boolean(this.#db.prepare(
      "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'store_totals'",
    ).get());
    const existingMeta = Boolean(this.#db.prepare(
      "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'store_meta'",
    ).get());
    const existingLabelBytesCursor = existingMeta && Boolean(this.#db.prepare(
      "SELECT 1 FROM store_meta WHERE key = 'label_bytes_backfill_cursor'",
    ).get());
    const existingSourceColumns = existingSources
      ? this.#db.prepare("SELECT name FROM pragma_table_info('sources')").all() as Array<{ name: string }>
      : [];
    const needsIndexedBytesBackfill = existingSources
      && !existingSourceColumns.some((column) => column.name === "indexed_bytes");
    if (needsIndexedBytesBackfill) {
      this.#db.exec("ALTER TABLE sources ADD COLUMN indexed_bytes INTEGER NOT NULL DEFAULT 0");
    }
    if (existingSources && !existingSourceColumns.some((column) => column.name === "totals_counted")) {
      this.#db.exec("ALTER TABLE sources ADD COLUMN totals_counted INTEGER NOT NULL DEFAULT 0");
    }

    this.#db.exec(`
      CREATE TABLE IF NOT EXISTS sources (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        label TEXT NOT NULL,
        chunk_count INTEGER NOT NULL DEFAULT 0,
        code_chunk_count INTEGER NOT NULL DEFAULT 0,
        indexed_bytes INTEGER NOT NULL DEFAULT 0,
        totals_counted INTEGER NOT NULL DEFAULT 1,
        indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
        file_path TEXT,
        content_hash TEXT
      );

      CREATE VIRTUAL TABLE IF NOT EXISTS chunks USING fts5(
        title,
        content,
        source_id UNINDEXED,
        content_type UNINDEXED,
        source_category UNINDEXED,
        session_id UNINDEXED,
        event_id UNINDEXED,
        timestamp UNINDEXED,
        tokenize='porter unicode61'
      );

      CREATE VIRTUAL TABLE IF NOT EXISTS chunks_trigram USING fts5(
        title,
        content,
        source_id UNINDEXED,
        content_type UNINDEXED,
        source_category UNINDEXED,
        session_id UNINDEXED,
        event_id UNINDEXED,
        timestamp UNINDEXED,
        tokenize='trigram'
      );

      CREATE TABLE IF NOT EXISTS vocabulary (
        word TEXT PRIMARY KEY
      );

      CREATE TABLE IF NOT EXISTS vocabulary_grams (
        gram TEXT NOT NULL,
        word TEXT NOT NULL,
        PRIMARY KEY (gram, word)
      );

      CREATE TABLE IF NOT EXISTS store_meta (
        key TEXT PRIMARY KEY,
        value INTEGER NOT NULL
      );

      CREATE TABLE IF NOT EXISTS store_owners (
        owner_id TEXT PRIMARY KEY,
        pid INTEGER NOT NULL,
        opened_at TEXT NOT NULL DEFAULT (datetime('now'))
      );

      CREATE TABLE IF NOT EXISTS retention_queue (
        source_id INTEGER PRIMARY KEY
      );

      CREATE TABLE IF NOT EXISTS store_totals (
        singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
        indexed_bytes INTEGER NOT NULL,
        chunks INTEGER NOT NULL,
        sources INTEGER NOT NULL
      );
      INSERT OR IGNORE INTO store_totals (singleton, indexed_bytes, chunks, sources)
      VALUES (1, 0, 0, 0);

      CREATE INDEX IF NOT EXISTS idx_sources_label ON sources(label);
      CREATE INDEX IF NOT EXISTS idx_sources_indexed_at_id ON sources(indexed_at, id);
      CREATE INDEX IF NOT EXISTS idx_vocabulary_grams_word ON vocabulary_grams(word);
    `);
    this.#db.prepare(`
      CREATE TRIGGER IF NOT EXISTS sources_totals_insert
      AFTER INSERT ON sources
      WHEN NEW.totals_counted = 1
      BEGIN
        UPDATE store_totals SET indexed_bytes = indexed_bytes + NEW.indexed_bytes,
          chunks = chunks + NEW.chunk_count, sources = sources + 1 WHERE singleton = 1;
      END
    `).run();
    this.#db.prepare(`
      CREATE TRIGGER IF NOT EXISTS sources_totals_delete
      AFTER DELETE ON sources
      WHEN OLD.totals_counted = 1
      BEGIN
        UPDATE store_totals SET indexed_bytes = MAX(0, indexed_bytes - OLD.indexed_bytes),
          chunks = MAX(0, chunks - OLD.chunk_count), sources = MAX(0, sources - 1) WHERE singleton = 1;
      END
    `).run();
    this.#db.prepare(`
      CREATE TRIGGER IF NOT EXISTS sources_totals_update
      AFTER UPDATE OF indexed_bytes, chunk_count ON sources
      WHEN OLD.totals_counted = 1 AND NEW.totals_counted = 1
      BEGIN
        UPDATE store_totals SET indexed_bytes = MAX(0, indexed_bytes + NEW.indexed_bytes - OLD.indexed_bytes),
          chunks = MAX(0, chunks + NEW.chunk_count - OLD.chunk_count) WHERE singleton = 1;
      END
    `).run();

    // FTS5 schema migration: old schema (4 cols) → new schema (8 cols).
    // FTS5 virtual tables do not support ALTER TABLE ADD COLUMN, so we must
    // DROP + re-CREATE. Detection: check for sentinel column `source_category`
    // via pragma_table_xinfo. Three states:
    //   1. No table          → CREATE above handled it (fresh DB)
    //   2. Old schema (4 cols) → DROP + CREATE new
    //   3. New schema (8 cols) → do nothing
    try {
      const cols = this.#db.prepare(
        "SELECT name FROM pragma_table_xinfo('chunks')"
      ).all() as Array<{ name: string }>;
      const colNames = new Set(cols.map(c => c.name));
      if (cols.length > 0 && !colNames.has("source_category")) {
        // Old schema detected — drop both FTS5 tables and re-create with new columns
        this.#db.exec("DROP TABLE IF EXISTS chunks");
        this.#db.exec("DROP TABLE IF EXISTS chunks_trigram");
        this.#db.exec(`
          CREATE VIRTUAL TABLE chunks USING fts5(
            title,
            content,
            source_id UNINDEXED,
            content_type UNINDEXED,
            source_category UNINDEXED,
            session_id UNINDEXED,
            event_id UNINDEXED,
            timestamp UNINDEXED,
            tokenize='porter unicode61'
          );
          CREATE VIRTUAL TABLE chunks_trigram USING fts5(
            title,
            content,
            source_id UNINDEXED,
            content_type UNINDEXED,
            source_category UNINDEXED,
            session_id UNINDEXED,
            event_id UNINDEXED,
            timestamp UNINDEXED,
            tokenize='trigram'
          );
        `);
      }
    } catch { /* pragma_table_xinfo may fail if table doesn't exist yet — safe to ignore */ }

    // Stale detection columns — safe for existing DBs (ALTER is O(1) in SQLite)
    try { this.#db.exec("ALTER TABLE sources ADD COLUMN file_path TEXT"); } catch { /* already exists */ }
    try { this.#db.exec("ALTER TABLE sources ADD COLUMN content_hash TEXT"); } catch { /* already exists */ }
    try { this.#db.exec("ALTER TABLE sources ADD COLUMN indexed_bytes INTEGER NOT NULL DEFAULT 0"); } catch { /* already exists */ }
    if (needsIndexedBytesBackfill) {
      const maxRow = this.#db.prepare("SELECT COALESCE(MAX(rowid), 0) AS rowid FROM chunks").get() as { rowid: number };
      this.#db.prepare("INSERT OR REPLACE INTO store_meta (key, value) VALUES ('indexed_bytes_backfill_cursor', 0)").run();
      this.#db.prepare("INSERT OR REPLACE INTO store_meta (key, value) VALUES ('indexed_bytes_backfill_max_rowid', ?)").run(maxRow.rowid);
      this.#db.prepare("UPDATE sources SET indexed_bytes = 0").run();
      this.#db.prepare("DELETE FROM vocabulary_grams").run();
      this.#db.prepare("DELETE FROM vocabulary").run();
    } else {
      this.#db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('indexed_bytes_backfill_cursor', -1)").run();
      this.#db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('indexed_bytes_backfill_max_rowid', -1)").run();
    }
    this.#db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('metadata_cursor', 0)").run();
    if (!existingLabelBytesCursor) {
      this.#db.prepare(
        "INSERT INTO store_meta (key, value) VALUES ('label_bytes_backfill_cursor', ?)",
      ).run(existingSources ? 0 : -1);
    }
    this.#db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('purge_generation', 0)").run();
    if (!existingSources) {
      this.#db.prepare("INSERT OR REPLACE INTO store_meta (key, value) VALUES ('totals_backfill_cursor', -1)").run();
    } else if (!existingTotals) {
      this.#db.prepare("UPDATE store_totals SET indexed_bytes = 0, chunks = 0, sources = 0 WHERE singleton = 1").run();
      this.#db.prepare("INSERT OR REPLACE INTO store_meta (key, value) VALUES ('totals_backfill_cursor', 0)").run();
    } else {
      this.#db.prepare("INSERT OR IGNORE INTO store_meta (key, value) VALUES ('totals_backfill_cursor', -1)").run();
    }
  }

  #prepareStatements(): void {
    // Write path
    this.#stmtInsertSourceEmpty = this.#db.prepare(
      "INSERT INTO sources (label, chunk_count, code_chunk_count, indexed_bytes, file_path, content_hash) VALUES (?, 0, 0, ?, ?, ?)",
    );
    this.#stmtInsertSource = this.#db.prepare(
      "INSERT INTO sources (label, chunk_count, code_chunk_count, indexed_bytes, file_path, content_hash) VALUES (?, ?, ?, ?, ?, ?)",
    );
    this.#stmtInsertChunk = this.#db.prepare(
      "INSERT INTO chunks (title, content, source_id, content_type, source_category, session_id, event_id, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    );
    this.#stmtInsertChunkTrigram = this.#db.prepare(
      "INSERT INTO chunks_trigram (title, content, source_id, content_type, source_category, session_id, event_id, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    );
    this.#stmtInsertVocab = this.#db.prepare(
      "INSERT OR IGNORE INTO vocabulary (word) VALUES (?)",
    );
    this.#stmtInsertVocabGram = this.#db.prepare(
      "INSERT OR IGNORE INTO vocabulary_grams (gram, word) VALUES (?, ?)",
    );

    // Dedup path: delete previous source with same label before re-indexing
    // Prevents stale outputs from accumulating in iterative workflows (build-fix-build)
    this.#stmtDeleteChunksByLabel = this.#db.prepare(
      "DELETE FROM chunks WHERE source_id IN (SELECT id FROM sources WHERE label = ?)",
    );
    this.#stmtDeleteChunksTrigramByLabel = this.#db.prepare(
      "DELETE FROM chunks_trigram WHERE source_id IN (SELECT id FROM sources WHERE label = ?)",
    );
    this.#stmtDeleteSourcesByLabel = this.#db.prepare(
      "DELETE FROM sources WHERE label = ?",
    );

    // Search path (hot)
    this.#stmtSearchPorter = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchPorterFiltered = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ? AND sources.label LIKE ? ESCAPE '\\'
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchPorterExact = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ? AND sources.label = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigram = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigramFiltered = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ? AND sources.label LIKE ? ESCAPE '\\'
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigramExact = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ? AND sources.label = ?
      ORDER BY rank
      LIMIT ?
    `);

    // Content-type filtered variants
    this.#stmtSearchPorterContentType = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ? AND chunks.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchPorterFilteredContentType = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ? AND sources.label LIKE ? ESCAPE '\\' AND chunks.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchPorterExactContentType = this.#db.prepare(`
      SELECT
        chunks.title,
        chunks.content,
        chunks.content_type,
        chunks.timestamp,
        sources.label,
        bm25(chunks, 5.0, 1.0) AS rank,
        highlight(chunks, 1, char(2), char(3)) AS highlighted,
        chunks.session_id
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      WHERE chunks MATCH ? AND sources.label = ? AND chunks.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigramContentType = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ? AND chunks_trigram.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigramFilteredContentType = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ? AND sources.label LIKE ? ESCAPE '\\' AND chunks_trigram.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);
    this.#stmtSearchTrigramExactContentType = this.#db.prepare(`
      SELECT
        chunks_trigram.title,
        chunks_trigram.content,
        chunks_trigram.content_type,
        chunks_trigram.timestamp,
        sources.label,
        bm25(chunks_trigram, 5.0, 1.0) AS rank,
        highlight(chunks_trigram, 1, char(2), char(3)) AS highlighted,
        chunks_trigram.session_id
      FROM chunks_trigram
      JOIN sources ON sources.id = chunks_trigram.source_id
      WHERE chunks_trigram MATCH ? AND sources.label = ? AND chunks_trigram.content_type = ?
      ORDER BY rank
      LIMIT ?
    `);

    // Read path
    this.#stmtListSources = this.#db.prepare(
      "SELECT label, chunk_count as chunkCount FROM sources ORDER BY id DESC LIMIT 1000",
    );
    this.#stmtChunksBySource = this.#db.prepare(
      `SELECT c.title, c.content, c.content_type, s.label
       FROM chunks c
       JOIN sources s ON s.id = c.source_id
       WHERE c.source_id = ?
       ORDER BY c.rowid`,
    );
    this.#stmtSourceChunkCount = this.#db.prepare(
      "SELECT chunk_count FROM sources WHERE id = ?",
    );
    this.#stmtChunkContent = this.#db.prepare(
      "SELECT content FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?",
    );
    this.#stmtVocabularyContent = this.#db.prepare(`
      SELECT chunks.content
      FROM chunks
      JOIN sources ON sources.id = chunks.source_id
      ORDER BY sources.id DESC, chunks.rowid
      LIMIT ?
    `);
    this.#stmtSourceMeta = this.#db.prepare(
      "SELECT label, chunk_count, code_chunk_count, indexed_at, file_path, content_hash FROM sources WHERE label = ?",
    );
    this.#stmtStats = this.#db.prepare(`
      SELECT
        (SELECT COUNT(*) FROM sources) AS sources,
        (SELECT COUNT(*) FROM chunks) AS chunks,
        (SELECT COUNT(*) FROM chunks WHERE content_type = 'code') AS codeChunks
    `);

    this.#stmtCleanupSource = this.#db.prepare(`
      SELECT id FROM sources
      WHERE indexed_at < datetime('now', '-' || ? || ' days')
      ORDER BY indexed_at, id LIMIT 1
    `);
    this.#stmtCleanupChunks = this.#db.prepare(`
      DELETE FROM chunks WHERE rowid IN (
        SELECT rowid FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?
      )
    `);
    this.#stmtCleanupChunksTrigram = this.#db.prepare(`
      DELETE FROM chunks_trigram WHERE rowid IN (
        SELECT rowid FROM chunks_trigram WHERE source_id = ? ORDER BY rowid LIMIT ?
      )
    `);
    this.#stmtCleanupSourceIfEmpty = this.#db.prepare(`
      DELETE FROM sources WHERE id = ?
        AND NOT EXISTS (SELECT 1 FROM chunks WHERE source_id = ? LIMIT 1)
        AND NOT EXISTS (SELECT 1 FROM chunks_trigram WHERE source_id = ? LIMIT 1)
    `);
  }

  // ── Deny Policy Hook ──

  /**
   * Register a deny-policy checker. When set, #refreshStaleSources
   * calls it before re-reading any file_path during auto-refresh.
   * Returning `true` causes the source to be skipped (kept in cache,
   * not re-indexed). server.ts wires this to the Read deny patterns.
   */
  setDenyChecker(fn: ((filePath: string) => boolean) | undefined): void {
    this.#denyChecker = fn;
  }

  // ── Index ──

  #purgeGeneration(): number {
    const row = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'purge_generation'").get() as { value: number };
    return row.value;
  }

  #assertInputSize(content: string): void {
    if (Buffer.byteLength(content) > MAX_INDEX_INPUT_BYTES) {
      throw new Error(`Content exceeds the ${MAX_INDEX_INPUT_BYTES}-byte indexing limit`);
    }
    let lines = 1;
    let offset = 0;
    while ((offset = content.indexOf("\n", offset)) >= 0) {
      if (++lines > MAX_INDEX_LINES) {
        throw new Error(`Content exceeds the ${MAX_INDEX_LINES}-line indexing limit`);
      }
      offset++;
    }
  }

  #assertSourceLabel(label: string): void {
    if (Buffer.byteLength(label) > MAX_SOURCE_LABEL_BYTES) {
      throw new Error(`Source label exceeds the ${MAX_SOURCE_LABEL_BYTES}-byte indexing limit`);
    }
  }

  #assertJSONTextBounds(content: string): void {
    let depth = 0;
    let nodes = 1;
    let inString = false;
    let escaped = false;
    for (let index = 0; index < content.length; index++) {
      const char = content[index];
      if (inString) {
        if (escaped) escaped = false;
        else if (char === "\\") escaped = true;
        else if (char === '"') inString = false;
        continue;
      }
      if (char === '"') inString = true;
      else if (char === "{" || char === "[") {
        if (++depth > MAX_JSON_DEPTH) throw new Error(`JSON exceeds the ${MAX_JSON_DEPTH}-level indexing limit`);
        if (++nodes > MAX_JSON_NODES) throw new Error(`JSON exceeds the ${MAX_JSON_NODES}-node indexing limit`);
      } else if (char === "}" || char === "]") depth--;
      else if (char === "," || char === ":") {
        if (++nodes > MAX_JSON_NODES) throw new Error(`JSON exceeds the ${MAX_JSON_NODES}-node indexing limit`);
      }
    }
  }

  index(options: {
    content?: string;
    path?: string;
    source?: string;
    /**
     * Optional FK metadata recorded on each indexed chunk so per-session
     * honest-savings stats can join chunks → session_events. When omitted,
     * chunks fall back to empty-string columns (legacy behaviour).
     */
    attribution?: { sessionId?: string; eventId?: string };
    expectedPurgeGeneration?: number;
  }): IndexResult {
    const { content, path, source, attribution } = options;
    const expectedPurgeGeneration = options.expectedPurgeGeneration ?? this.#purgeGeneration();
    const label = source ?? path ?? "untitled";
    this.#assertSourceLabel(label);

    // Treat empty string as "no content" so an empty `content` paired with a
    // valid `path` falls back to reading the file. Some MCP clients
    // materialize optional string fields as `""` and the previous
    // `content ?? readFileSync(path)` kept the empty string, indexing 0
    // chunks. See issue #350.
    const hasContent = typeof content === "string" && content.length > 0;

    if (!hasContent && !path) {
      throw new Error("Either content or path must be provided");
    }

    // Read file via fd to close the TOCTOU window between the security
    // gate (security.ts evaluateFilePath calls realpathSync) and the read
    // here. Lexical re-read by path string allowed an attacker to swap a
    // symlink to a denied target (e.g. ~/.ssh/id_rsa) AFTER gate passed.
    // openSync + fstat + readFileSync(fd) binds the read to the inode
    // captured at gate-time. fstat also rejects non-regular files
    // (directories, character devices) which would otherwise read as ""
    // or throw inconsistently. See #442 round-3.
    let text: string;
    if (hasContent) {
      this.#assertInputSize(content!);
      text = content!;
    } else {
      const fd = openSync(path!, "r");
      try {
        const st = fstatSync(fd);
        if (!st.isFile()) {
          throw new Error(`refusing to index ${path}: not a regular file`);
        }
        if (st.size > MAX_INDEX_INPUT_BYTES) {
          throw new Error(`File exceeds the ${MAX_INDEX_INPUT_BYTES}-byte indexing limit`);
        }
        text = readFileSync(fd, "utf-8");
        this.#assertInputSize(text);
      } finally {
        closeSync(fd);
      }
    }
    const chunks = this.#chunkMarkdown(text);

    // Stale detection: store file_path + SHA-256 for file-backed sources
    const filePath = path ?? undefined;
    const contentHash = filePath ? createHash("sha256").update(text).digest("hex") : undefined;

    return this.#insertChunks(chunks, label, text, filePath, contentHash, attribution, expectedPurgeGeneration);
  }

  // ── Index Directory (#687) ──

  /**
   * Index every file under a directory by walking it with `walkDirectory` and
   * delegating each discovered file to `this.index({ path })`. The per-file
   * `openSync + fstatSync.isFile()` security gate at line ~845 stays active
   * for every file — directory support never bypasses the TOCTOU defense
   * from #442 round-3.
   *
   * Reported by @matiasduartee in #687.
   */
  indexDirectory(opts: {
    path: string;
    source?: string;
    attribution?: { sessionId?: string; eventId?: string };
    /** Optional per-file deny check — runs INSIDE the walk loop so a denied
     *  file does not even open a fd. Returns true to deny. */
    perFileDeny?: (absPath: string) => boolean;
  } & WalkOptions): {
    filesIndexed: number;
    totalChunks: number;
    capped: boolean;
    totalSeen: number;
    denied: number;
    failed: number;
    label: string;
  } {
    const { path: rootPath, source, attribution, perFileDeny, ...walkOpts } = opts;
    const expectedPurgeGeneration = this.#purgeGeneration();
    const walked = walkDirectoryDetailed(rootPath, walkOpts);

    let filesIndexed = 0;
    let totalChunks = 0;
    let denied = 0;
    let failed = 0;

    for (const file of walked.files) {
      if (perFileDeny && perFileDeny(file)) {
        denied++;
        continue;
      }
      try {
        // Per-file source label so ctx_search(source: "<file>") still works.
        const fileSource = source ? `${source}:${file}` : file;
        const r = this.index({
          path: file,
          source: fileSource,
          attribution,
          expectedPurgeGeneration,
        });
        filesIndexed++;
        totalChunks += r.totalChunks;
      } catch {
        // Per-file failure (e.g. fd-bound fstat rejection of a non-regular
        // file that races between walk and read) — count + continue.
        failed++;
      }
    }

    return {
      filesIndexed,
      totalChunks,
      capped: walked.capped,
      totalSeen: walked.totalSeen,
      denied,
      failed,
      label: source ?? rootPath,
    };
  }

  // ── Index Plain Text ──

  /**
   * Index plain-text output (logs, build output, test results) by splitting
   * into fixed-size line groups. Unlike markdown indexing, this does not
   * look for headings — it chunks by line count with overlap.
   */
  indexPlainText(
    content: string,
    source: string,
    linesPerChunk: number = 20,
    attribution?: { sessionId?: string; eventId?: string },
    maxChunkBytes: number = MAX_CHUNK_BYTES,
    expectedPurgeGeneration: number = this.#purgeGeneration(),
  ): IndexResult {
    this.#assertSourceLabel(source);
    this.#assertInputSize(content);
    if (!content || content.trim().length === 0) {
      return this.#insertChunks([], source, "", undefined, undefined, attribution, expectedPurgeGeneration);
    }

    const chunks = this.#chunkPlainText(content, linesPerChunk, maxChunkBytes);

    return this.#insertChunks(
      chunks.map((c) => ({ ...c, hasCode: false })),
      source,
      content,
      undefined,
      undefined,
      attribution,
      expectedPurgeGeneration,
    );
  }

  // ── Index JSON ──

  /**
   * Index JSON content by walking the object tree and using key paths
   * as chunk titles (analogous to heading hierarchy in markdown). Objects
   * recurse by key; arrays batch items by size.
   *
   * Falls back to `indexPlainText` if the content is not valid JSON.
   */
  indexJSON(
    content: string,
    source: string,
    maxChunkBytes: number = MAX_CHUNK_BYTES,
    attribution?: { sessionId?: string; eventId?: string },
    expectedPurgeGeneration: number = this.#purgeGeneration(),
  ): IndexResult {
    this.#assertSourceLabel(source);
    this.#assertInputSize(content);
    if (!content || content.trim().length === 0) {
      return this.indexPlainText("", source, undefined, attribution, maxChunkBytes, expectedPurgeGeneration);
    }

    this.#assertJSONTextBounds(content);
    let parsed: unknown;
    try {
      parsed = JSON.parse(content);
    } catch {
      return this.indexPlainText(content, source, undefined, attribution, maxChunkBytes, expectedPurgeGeneration);
    }

    const chunks: Chunk[] = [];
    this.#walkJSON(parsed, [], chunks, maxChunkBytes);

    if (chunks.length === 0) {
      return this.indexPlainText(content, source, undefined, attribution, maxChunkBytes, expectedPurgeGeneration);
    }

    return this.#insertChunks(chunks, source, content, undefined, undefined, attribution, expectedPurgeGeneration);
  }

  // ── Shared DB Insertion ──

  /**
   * Shared DB insertion logic for all index methods. Inserts chunks
   * into both FTS5 tables within a transaction and extracts vocabulary.
   * Uses cached prepared statements from #prepareStatements().
   */
  #insertChunks(
    chunks: Chunk[],
    label: string,
    _text: string,
    filePath: string | undefined,
    contentHash: string | undefined,
    attribution: { sessionId?: string; eventId?: string } | undefined,
    expectedPurgeGeneration: number,
  ): IndexResult {
    this.#assertSourceLabel(label);
    this.#runMaintenanceBatch(false);
    const boundedChunks: Chunk[] = [];
    for (const chunk of chunks) {
      if (Buffer.byteLength(chunk.content) <= MAX_CHUNK_BYTES) {
        boundedChunks.push(chunk);
      } else {
        for (const part of this.#splitOversizedPlainChunk([chunk.content], chunk.title, MAX_CHUNK_BYTES)) {
          boundedChunks.push({ ...part, hasCode: chunk.hasCode });
          if (boundedChunks.length >= this.#limits.maxChunks) break;
        }
      }
      if (boundedChunks.length >= this.#limits.maxChunks) break;
    }
    const candidateChunks = boundedChunks.slice(0, this.#limits.maxChunks);
    // FK columns on chunks. Empty-string fallback preserves the FTS5-friendly
    // "not-null but unattributed" sentinel used by legacy rows.
    const sessionIdCol = attribution?.sessionId ?? "";
    const eventIdCol = attribution?.eventId ?? "";

    // Atomic dedup + insert: delete previous source with same label,
    // then insert new content — all within a single transaction.
    // Prevents stale results in iterative workflows. (See: GitHub issue #67)
    const transaction = this.#db.transaction(() => {
      const generation = this.#purgeGeneration();
      if (generation !== expectedPurgeGeneration) {
        throw new Error("Indexing aborted because the content store was purged during processing");
      }
      const migration = this.#db.prepare(`
        SELECT
          (SELECT value FROM store_meta WHERE key = 'indexed_bytes_backfill_cursor') AS bytes_cursor,
          (SELECT value FROM store_meta WHERE key = 'label_bytes_backfill_cursor') AS label_cursor,
          (SELECT value FROM store_meta WHERE key = 'totals_backfill_cursor') AS totals_cursor
      `).get() as { bytes_cursor: number; label_cursor: number; totals_cursor: number };
      if (migration.bytes_cursor >= 0 || migration.label_cursor >= 0 || migration.totals_cursor >= 0) {
        throw new Error("Indexing deferred until bounded content-store migration completes");
      }
      const totals = this.#db.prepare(
        "SELECT indexed_bytes, chunks, sources FROM store_totals WHERE singleton = 1",
      ).get() as { indexed_bytes: number; chunks: number; sources: number };
      const replaced = this.#db.prepare(`
        SELECT COUNT(*) AS sources, COALESCE(SUM(indexed_bytes), 0) AS indexed_bytes,
          COALESCE(SUM(chunk_count), 0) AS chunks
        FROM sources WHERE label = ?
      `).get(label) as { sources: number; indexed_bytes: number; chunks: number };
      const replacing = replaced.sources > 0;
      let retainedChunks: Chunk[] = [];
      let indexedBytes = Buffer.byteLength(label);
      for (const chunk of candidateChunks) {
        const chunkBytes = Buffer.byteLength(chunk.title) + Buffer.byteLength(chunk.content);
        if (retainedChunks.length >= this.#limits.maxChunks
          || indexedBytes + chunkBytes > this.#limits.maxIndexedBytes) break;
        retainedChunks.push(chunk);
        indexedBytes += chunkBytes;
      }

      let currentChunks = totals.chunks - replaced.chunks;
      let currentBytes = totals.indexed_bytes - replaced.indexed_bytes;
      let currentSources = totals.sources - replaced.sources;
      let vocabularyChanged = replacing;
      const capacityExceeded = () => currentChunks + retainedChunks.length > this.#limits.maxChunks
        || currentBytes + indexedBytes > this.#limits.maxIndexedBytes
        || currentSources + 1 > this.#limits.maxSources;
      if (capacityExceeded()) {
        const oldest = this.#db.prepare(`
          SELECT id, chunk_count, indexed_bytes FROM sources
          WHERE label <> ? ORDER BY id LIMIT 1
        `).get(label) as { id: number; chunk_count: number; indexed_bytes: number } | undefined;
        if (oldest) {
          const deleteLimit = Math.min(oldest.chunk_count, this.#limits.maintenanceChunkBatch);
          const removed = this.#db.prepare(`
            SELECT COUNT(*) AS chunks,
              COALESCE(SUM(length(CAST(title AS BLOB)) + length(CAST(content AS BLOB))), 0) AS bytes
            FROM (SELECT title, content FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?)
          `).get(oldest.id, deleteLimit) as { chunks: number; bytes: number };
          this.#db.prepare(`
            DELETE FROM chunks WHERE rowid IN (
              SELECT rowid FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?
            )
          `).run(oldest.id, deleteLimit);
          this.#db.prepare(`
            DELETE FROM chunks_trigram WHERE rowid IN (
              SELECT rowid FROM chunks_trigram WHERE source_id = ? ORDER BY rowid LIMIT ?
            )
          `).run(oldest.id, deleteLimit);
          currentChunks -= removed.chunks;
          currentBytes -= removed.bytes;
          vocabularyChanged ||= removed.chunks > 0;
          const remainingChunks = oldest.chunk_count - removed.chunks;
          if (remainingChunks <= 0) {
            currentBytes -= Math.max(0, oldest.indexed_bytes - removed.bytes);
            this.#db.prepare("DELETE FROM retention_queue WHERE source_id = ?").run(oldest.id);
            this.#db.prepare("DELETE FROM sources WHERE id = ?").run(oldest.id);
            currentSources--;
          } else {
            this.#db.prepare(`
              UPDATE sources SET chunk_count = ?, indexed_bytes = MAX(0, indexed_bytes - ?),
                code_chunk_count = (SELECT COUNT(*) FROM chunks WHERE source_id = ? AND content_type = 'code')
              WHERE id = ?
            `).run(remainingChunks, removed.bytes, oldest.id, oldest.id);
          }
        }
      }

      if (capacityExceeded()) {
        if (vocabularyChanged) this.#rebuildVocabulary();
        this.#scheduleRetention();
        return { deferred: true as const };
      }

      const availableChunks = Math.max(0, this.#limits.maxChunks - currentChunks);
      const availableBytes = Math.max(0, this.#limits.maxIndexedBytes - currentBytes);
      if (retainedChunks.length > availableChunks || indexedBytes > availableBytes) {
        const admitted: Chunk[] = [];
        indexedBytes = Buffer.byteLength(label);
        for (const chunk of retainedChunks) {
          const chunkBytes = Buffer.byteLength(chunk.title) + Buffer.byteLength(chunk.content);
          if (admitted.length >= availableChunks || indexedBytes + chunkBytes > availableBytes) break;
          admitted.push(chunk);
          indexedBytes += chunkBytes;
        }
        retainedChunks = admitted;
      }
      const codeChunks = retainedChunks.filter((chunk) => chunk.hasCode).length;
      this.#db.prepare("DELETE FROM retention_queue WHERE source_id IN (SELECT id FROM sources WHERE label = ?)").run(label);
      this.#stmtDeleteChunksByLabel.run(label);
      this.#stmtDeleteChunksTrigramByLabel.run(label);
      this.#stmtDeleteSourcesByLabel.run(label);

      if (retainedChunks.length === 0) {
        const info = this.#stmtInsertSourceEmpty.run(label, indexedBytes, filePath ?? null, contentHash ?? null);
        if (vocabularyChanged) this.#rebuildVocabulary();
        this.#scheduleRetention();
        return {
          sourceId: Number(info.lastInsertRowid),
          totalChunks: 0,
          codeChunks: 0,
        };
      }

      const info = this.#stmtInsertSource.run(label, retainedChunks.length, codeChunks, indexedBytes, filePath ?? null, contentHash ?? null);
      const sourceId = Number(info.lastInsertRowid);

      const now = new Date().toISOString();
      for (const chunk of retainedChunks) {
        const ct = chunk.hasCode ? "code" : "prose";
        this.#stmtInsertChunk.run(chunk.title, chunk.content, sourceId, ct, null, sessionIdCol, eventIdCol, now);
        this.#stmtInsertChunkTrigram.run(chunk.title, chunk.content, sourceId, ct, null, sessionIdCol, eventIdCol, now);
      }

      if (vocabularyChanged) this.#rebuildVocabulary();
      else this.#addVocabulary(retainedChunks);
      this.#scheduleRetention();
      return { sourceId, totalChunks: retainedChunks.length, codeChunks };
    });

    const result = withRetry(() => transaction(), [10, 50, 200]);
    this.#fuzzyCache.clear();
    this.#writesSinceCheckpoint++;
    this.#checkpointWALIfNeeded();
    if ("deferred" in result) {
      throw new Error("Indexing deferred until bounded retention maintenance completes");
    }

    // Periodically optimize FTS5 indexes to merge b-tree segments.
    // Fragmentation accumulates over insert/delete cycles (dedup re-indexes
    // every source on update). The 'optimize' command merges segments into
    // a single b-tree, improving search latency for long-running sessions.
    this.#insertCount++;
    if (this.#insertCount % ContentStore.OPTIMIZE_EVERY === 0) {
      this.#optimizeFTS();
    }

    return {
      sourceId: result.sourceId,
      label,
      totalChunks: result.totalChunks,
      codeChunks: result.codeChunks,
    };
  }

  // ── Search ──

  #mapSearchRows(rows: SearchRow[]): SearchResult[] {
    return rows.map((r) => ({
      title: r.title,
      content: r.content,
      source: r.label,
      rank: r.rank,
      contentType: r.content_type as "code" | "prose",
      highlighted: r.highlighted,
      timestamp: r.timestamp ?? undefined,
      sessionId: r.session_id ?? "",
    }));
  }

  #sourceFilterParam(source: string, sourceMatchMode: SourceMatchMode): string {
    if (sourceMatchMode === "exact") return source;
    // Escape SQLite LIKE metacharacters so user-supplied source labels
    // containing `_`, `%`, or `\` are matched literally rather than as
    // wildcards. Backslash must be replaced first (otherwise subsequent
    // escapes would themselves be re-escaped). Paired with `ESCAPE '\'`
    // in the four prepared LIKE statements (#stmtSearchPorter*,
    // #stmtSearchTrigram*). Regression: #646.
    const escaped = source
      .replace(/\\/g, "\\\\")
      .replace(/%/g, "\\%")
      .replace(/_/g, "\\_");
    return `%${escaped}%`;
  }

  #searchWithSessionFilter(
    table: "chunks" | "chunks_trigram",
    sanitized: string,
    limit: number,
    allowSet: Set<string>,
    source?: string,
    contentType?: "code" | "prose",
    sourceMatchMode: SourceMatchMode = "like",
  ): SearchResult[] {
    const conditions = [
      `${table} MATCH ?`,
      `(${table}.session_id = '' OR ${table}.session_id IN (SELECT value FROM json_each(?)))`,
    ];
    const params: unknown[] = [sanitized, JSON.stringify([...allowSet])];
    if (source) {
      conditions.push(sourceMatchMode === "exact"
        ? "sources.label = ?"
        : "sources.label LIKE ? ESCAPE '\\'");
      params.push(this.#sourceFilterParam(source, sourceMatchMode));
    }
    if (contentType) {
      conditions.push(`${table}.content_type = ?`);
      params.push(contentType);
    }
    params.push(limit);
    const statement = this.#db.prepare(`
      SELECT
        ${table}.title,
        ${table}.content,
        ${table}.content_type,
        ${table}.timestamp,
        sources.label,
        bm25(${table}, 5.0, 1.0) AS rank,
        highlight(${table}, 1, char(2), char(3)) AS highlighted,
        ${table}.session_id
      FROM ${table}
      JOIN sources ON sources.id = ${table}.source_id
      WHERE ${conditions.join(" AND ")}
      ORDER BY rank
      LIMIT ?
    `) as unknown as PreparedStatement;
    try {
      return withRetry(() => this.#mapSearchRows(statement.all(...params) as SearchRow[]));
    } finally {
      statement.finalize?.();
    }
  }

  search(
    query: string,
    limit: number = 3,
    source?: string,
    mode: "AND" | "OR" = "AND",
    contentType?: "code" | "prose",
    sourceMatchMode: SourceMatchMode = "like",
    sessionIdAllowSet?: Set<string>,
  ): SearchResult[] {
    const sanitized = sanitizeQuery(query, mode);
    if (sessionIdAllowSet) {
      return this.#searchWithSessionFilter(
        "chunks", sanitized, limit, sessionIdAllowSet, source, contentType, sourceMatchMode,
      );
    }

    let stmt: PreparedStatement;
    let params: unknown[];

    if (source && contentType) {
      stmt = sourceMatchMode === "exact"
        ? this.#stmtSearchPorterExactContentType
        : this.#stmtSearchPorterFilteredContentType;
      params = [sanitized, this.#sourceFilterParam(source, sourceMatchMode), contentType, limit];
    } else if (source) {
      stmt = sourceMatchMode === "exact"
        ? this.#stmtSearchPorterExact
        : this.#stmtSearchPorterFiltered;
      params = [sanitized, this.#sourceFilterParam(source, sourceMatchMode), limit];
    } else if (contentType) {
      stmt = this.#stmtSearchPorterContentType;
      params = [sanitized, contentType, limit];
    } else {
      stmt = this.#stmtSearchPorter;
      params = [sanitized, limit];
    }

    return withRetry(() => this.#mapSearchRows(stmt.all(...params) as SearchRow[]));
  }

  // ── Trigram Search (Layer 2) ──

  searchTrigram(
    query: string,
    limit: number = 3,
    source?: string,
    mode: "AND" | "OR" = "AND",
    contentType?: "code" | "prose",
    sourceMatchMode: SourceMatchMode = "like",
    sessionIdAllowSet?: Set<string>,
  ): SearchResult[] {
    const sanitized = sanitizeTrigramQuery(query, mode);
    if (!sanitized) return [];
    if (sessionIdAllowSet) {
      return this.#searchWithSessionFilter(
        "chunks_trigram", sanitized, limit, sessionIdAllowSet, source, contentType, sourceMatchMode,
      );
    }

    let stmt: PreparedStatement;
    let params: unknown[];

    if (source && contentType) {
      stmt = sourceMatchMode === "exact"
        ? this.#stmtSearchTrigramExactContentType
        : this.#stmtSearchTrigramFilteredContentType;
      params = [sanitized, this.#sourceFilterParam(source, sourceMatchMode), contentType, limit];
    } else if (source) {
      stmt = sourceMatchMode === "exact"
        ? this.#stmtSearchTrigramExact
        : this.#stmtSearchTrigramFiltered;
      params = [sanitized, this.#sourceFilterParam(source, sourceMatchMode), limit];
    } else if (contentType) {
      stmt = this.#stmtSearchTrigramContentType;
      params = [sanitized, contentType, limit];
    } else {
      stmt = this.#stmtSearchTrigram;
      params = [sanitized, limit];
    }

    return withRetry(() => this.#mapSearchRows(stmt.all(...params) as SearchRow[]));
  }

  // ── Fuzzy Correction (Layer 3) ──

  #readDataVersion(): number {
    const raw = this.#db.pragma("data_version") as unknown;
    if (typeof raw === "number") return raw;
    const row = (Array.isArray(raw) ? raw[0] : raw) as { data_version?: number } | undefined;
    return row?.data_version ?? -1;
  }

  fuzzyCorrect(query: string): string | null {
    const dataVersion = this.#readDataVersion();
    if (dataVersion !== this.#fuzzyDataVersion) {
      this.#fuzzyCache.clear();
      this.#fuzzyDataVersion = dataVersion;
    }
    const word = query.toLowerCase().trim();
    if (word.length < 3 || word.length > this.#limits.maxFuzzyWordLength) return null;

    // Cache hit: promote to tail (Map preserves insertion order → LRU).
    if (this.#fuzzyCache.has(word)) {
      const cached = this.#fuzzyCache.get(word) ?? null;
      this.#fuzzyCache.delete(word);
      this.#fuzzyCache.set(word, cached);
      return cached;
    }

    const maxDist = maxEditDistance(word.length);

    const grams = vocabularyGrams(word);
    const placeholders = grams.map(() => "?").join(", ");
    const candidates = this.#db.prepare(`
      SELECT word
      FROM vocabulary_grams
      WHERE gram IN (${placeholders})
        AND length(word) BETWEEN ? AND ?
      GROUP BY word
      ORDER BY COUNT(*) DESC, abs(length(word) - ?), word
      LIMIT ?
    `).all(
      ...grams,
      word.length - maxDist,
      word.length + maxDist,
      word.length,
      this.#limits.maxFuzzyCandidates,
    ) as Array<{ word: string }>;
    this.lastFuzzyCandidateCount = candidates.length;

    let bestWord: string | null = null;
    let bestDist = maxDist + 1;
    let exactMatch = false;

    for (const { word: candidate } of candidates) {
      if (candidate === word) {
        exactMatch = true;
        break;
      }
      const dist = levenshtein(word, candidate);
      if (dist < bestDist) {
        bestDist = dist;
        bestWord = candidate;
      }
    }

    const result = exactMatch ? null : bestDist <= maxDist ? bestWord : null;

    // Evict the oldest entry before insert if we hit the size cap.
    if (this.#fuzzyCache.size >= ContentStore.FUZZY_CACHE_SIZE) {
      const oldestKey = this.#fuzzyCache.keys().next().value;
      if (oldestKey !== undefined) this.#fuzzyCache.delete(oldestKey);
    }
    this.#fuzzyCache.set(word, result);

    return result;
  }

  // ── Reciprocal Rank Fusion (Cormack et al. 2009) ──

  #rrfSearch(
    query: string,
    limit: number,
    source?: string,
    contentType?: "code" | "prose",
    sourceMatchMode: SourceMatchMode = "like",
    sessionIdAllowSet?: Set<string>,
  ): SearchResult[] {
    const K = 60; // Standard RRF constant
    const fetchLimit = Math.max(limit * 2, 10);

    const porterResults = this.search(query, fetchLimit, source, "OR", contentType, sourceMatchMode, sessionIdAllowSet);
    const trigramResults = this.searchTrigram(query, fetchLimit, source, "OR", contentType, sourceMatchMode, sessionIdAllowSet);

    const scoreMap = new Map<string, { result: SearchResult; score: number }>();
    const key = (r: SearchResult) => `${r.source}::${r.title}`;

    for (const [i, r] of porterResults.entries()) {
      const k = key(r);
      const existing = scoreMap.get(k);
      if (existing) {
        existing.score += 1 / (K + i + 1);
      } else {
        scoreMap.set(k, { result: r, score: 1 / (K + i + 1) });
      }
    }

    for (const [i, r] of trigramResults.entries()) {
      const k = key(r);
      const existing = scoreMap.get(k);
      if (existing) {
        existing.score += 1 / (K + i + 1);
      } else {
        scoreMap.set(k, { result: r, score: 1 / (K + i + 1) });
      }
    }

    return Array.from(scoreMap.values())
      .sort((a, b) => b.score - a.score)
      .slice(0, limit)
      .map(({ result, score }) => ({ ...result, rank: -score }));
  }

  // ── Proximity Reranking ──

  #applyProximityReranking(
    results: SearchResult[],
    query: string,
  ): SearchResult[] {
    const allTerms = query
      .toLowerCase()
      .split(/\s+/)
      .filter((w) => w.length >= 2);
    // Exclude stopwords from proximity/title scoring — they match everywhere
    // and inflate boosts for irrelevant chunks. Keep all terms as fallback.
    const filtered = allTerms.filter((w) => !STOPWORDS.has(w));
    const terms = filtered.length > 0 ? filtered : allTerms;

    return results
      .map((r) => {
        // Title-match boost: query terms found in the chunk title get a boost.
        // Code chunks get a stronger title boost (function/class names are high
        // signal) while prose chunks get a moderate one (headings are useful but
        // body carries more weight).
        const titleLower = r.title.toLowerCase();
        const titleHits = terms.filter((t) => titleLower.includes(t)).length;
        const titleWeight = r.contentType === "code" ? 0.6 : 0.3;
        const titleBoost = titleHits > 0 ? titleWeight * (titleHits / terms.length) : 0;

        // Proximity boost for multi-term queries. minSpan picks the single
        // tightest window — frequency doesn't move it, so a long doc with one
        // tight occurrence outranks a short doc with several. Phrase-frequency
        // reward layers a saturating frequency signal on top: cap 0.5 (below
        // proximity max ≈1.0, in title-boost range), saturates at 4 hits.
        let proximityBoost = 0;
        let phraseBoost = 0;
        if (terms.length >= 2) {
          const content = r.content.toLowerCase();
          const positions = terms.map((t) => findAllPositions(content, t));

          if (!positions.some((p) => p.length === 0)) {
            const minSpan = findMinSpan(positions);
            proximityBoost = 1 / (1 + minSpan / Math.max(content.length, 1));

            const adjacentPairs = countAdjacentPairs(positions, terms);
            phraseBoost = 0.5 * Math.min(1, adjacentPairs / 4);
          }
        }

        return { result: r, boost: titleBoost + proximityBoost + phraseBoost };
      })
      .sort((a, b) => b.boost - a.boost || a.result.rank - b.result.rank)
      .map(({ result }) => result);
  }

  // ── Unified Fallback Search ──

  searchWithFallback(
    query: string,
    limit: number = 3,
    source?: string,
    contentType?: "code" | "prose",
    sourceMatchMode: SourceMatchMode = "like",
    sessionIdAllowSet?: Set<string>,
  ): SearchResult[] {
    // Step 0: Advance bounded storage maintenance, then refresh stale sources.
    this.#runMaintenanceBatch();
    this.#refreshStaleSources();

    // Step 1: RRF fusion (porter OR + trigram OR → merge)
    const rrfResults = this.#rrfSearch(
      query, limit, source, contentType, sourceMatchMode, sessionIdAllowSet,
    );
    if (rrfResults.length > 0) {
      const reranked = this.#applyProximityReranking(rrfResults.slice(0, limit), query);
      return reranked.map((r) => ({ ...r, matchLayer: "rrf" as const }));
    }

    // Step 2: Fuzzy correction → RRF re-run
    // Skip stopwords — they'll be filtered by sanitizeQuery anyway, and each
    // fuzzyCorrect call hits the vocab DB + runs levenshtein comparisons.
    const words = query
      .toLowerCase()
      .trim()
      .split(/\s+/)
      .filter((w) => w.length >= 3 && !STOPWORDS.has(w))
      .slice(0, this.#limits.maxFuzzyQueryWords);
    const original = words.join(" ");
    const correctedWords = words.map((w) => this.fuzzyCorrect(w) ?? w);
    const correctedQuery = correctedWords.join(" ");

    if (correctedQuery !== original) {
      const fuzzyResults = this.#rrfSearch(
        correctedQuery, limit, source, contentType, sourceMatchMode, sessionIdAllowSet,
      );
      if (fuzzyResults.length > 0) {
        const reranked = this.#applyProximityReranking(fuzzyResults.slice(0, limit), correctedQuery);
        return reranked.map((r) => ({ ...r, matchLayer: "rrf-fuzzy" as const }));
      }
    }

    return [];
  }

  /** Number of sources auto-refreshed in the last searchWithFallback call. */
  lastRefreshCount = 0;

  /**
   * Check all file-backed sources for staleness and auto re-index changed files.
   * Uses mtime as a fast gate — only computes SHA-256 when mtime has advanced
   * past indexed_at. Gracefully skips deleted files and non-file sources.
   */
  #refreshStaleSources(): void {
    this.lastRefreshCount = 0;
    const expectedPurgeGeneration = this.#purgeGeneration();
    const cursor = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'metadata_cursor'").get() as { value: number };
    const statement = this.#db.prepare(
      "SELECT id, label, file_path, content_hash, indexed_at FROM sources WHERE file_path IS NOT NULL AND id > ? ORDER BY id LIMIT ?",
    );
    let sources = statement.all(cursor.value, this.#limits.maxMetadataChecks) as Array<{
      id: number; label: string; file_path: string; content_hash: string; indexed_at: string;
    }>;
    if (sources.length === 0 && cursor.value > 0) {
      sources = statement.all(0, this.#limits.maxMetadataChecks) as typeof sources;
    }
    if (sources.length > 0) {
      this.#db.prepare("UPDATE store_meta SET value = ? WHERE key = 'metadata_cursor'").run(sources.at(-1)!.id);
    }
    this.lastMetadataCheckCount = sources.length;

    for (const src of sources) {
      try {
        if (!existsSync(src.file_path)) continue; // file deleted — keep cached results
        // Re-check deny policy before re-reading. The Read deny list may
        // have been edited after this source was originally indexed; a
        // file that was allowed then may now be denied. Without this
        // gate, refresh would happily re-read and re-expose it. #442 r3.
        if (this.#denyChecker && this.#denyChecker(src.file_path)) continue;
        const mtime = statSync(src.file_path).mtime;
        const indexedAt = new Date(src.indexed_at + "Z");
        if (mtime <= indexedAt) continue; // file unchanged — fast path

        // mtime advanced — fd-bound read for hash + indexing in one go.
        // Open once, fstat, read from fd. Closes the swap-mid-flight
        // window between hash read and re-index. #442 round-3.
        const fd = openSync(src.file_path, "r");
        let newContent: string;
        try {
          const st = fstatSync(fd);
          if (!st.isFile() || st.size > MAX_INDEX_INPUT_BYTES) continue;
          newContent = readFileSync(fd, "utf-8");
          this.#assertInputSize(newContent);
        } finally {
          closeSync(fd);
        }
        const newHash = createHash("sha256").update(newContent).digest("hex");
        if (newHash === src.content_hash) continue; // content identical — skip

        // File genuinely changed — re-index using already-read content
        // (avoids a second open/read race) but preserve file_path/hash
        // by going through index() which stores them. Since we pass
        // content, index() does NOT re-read; the bytes hashed above
        // are exactly the bytes indexed.
        this.index({
          content: newContent,
          path: src.file_path,
          source: src.label,
          expectedPurgeGeneration,
        });
        this.lastRefreshCount++;
      } catch {
        // Graceful degradation — never break search for stale detection
      }
    }
  }

  // ── Sources ──

  getSourceMeta(label: string): { label: string; chunkCount: number; codeChunkCount: number; indexedAt: string; filePath: string | null; contentHash: string | null } | null {
    const row = this.#stmtSourceMeta.get(label) as { label: string; chunk_count: number; code_chunk_count: number; indexed_at: string; file_path: string | null; content_hash: string | null } | undefined;
    if (!row) return null;
    return { label: row.label, chunkCount: row.chunk_count, codeChunkCount: row.code_chunk_count, indexedAt: row.indexed_at, filePath: row.file_path ?? null, contentHash: row.content_hash ?? null };
  }

  listSources(): Array<{ label: string; chunkCount: number }> {
    return this.#stmtListSources.all() as Array<{
      label: string;
      chunkCount: number;
    }>;
  }

  /**
   * Aggregate snapshot of the persistent content store. Returns total
   * chunk count, source count, and the most recent indexed_at timestamp.
   * Used by ctx_stats so callers can see observability state in the same
   * round trip instead of inferring it from snapshot diffs.
   */
  getIndexState(): { totalChunks: number; totalSources: number; lastIndexedAt?: string } {
    const row = (this.#db
      .prepare("SELECT COALESCE(SUM(chunk_count), 0) AS total_chunks, COUNT(*) AS total_sources, MAX(indexed_at) AS last_indexed_at FROM sources")
      .get() as {
        total_chunks: number;
        total_sources: number;
        last_indexed_at: string | null;
      });
    return {
      totalChunks: row.total_chunks ?? 0,
      totalSources: row.total_sources ?? 0,
      lastIndexedAt: row.last_indexed_at ?? undefined,
    };
  }

  /**
   * Get all chunks for a given source by ID — bypasses FTS5 MATCH entirely.
   * Use this for inventory/listing where you need all sections, not search.
   */
  getChunksBySource(sourceId: number): SearchResult[] {
    const rows = this.#stmtChunksBySource.all(sourceId) as Array<{
      title: string;
      content: string;
      content_type: string;
      label: string;
    }>;

    return rows.map((r) => ({
      title: r.title,
      content: r.content,
      source: r.label,
      rank: 0,
      contentType: r.content_type as "code" | "prose",
    }));
  }

  // ── Vocabulary ──

  getDistinctiveTerms(sourceId: number, maxTerms: number = 40): string[] {
    const stats = this.#stmtSourceChunkCount.get(sourceId) as
      | { chunk_count: number }
      | undefined;

    if (!stats || stats.chunk_count < 3) return [];

    const totalChunks = Math.min(stats.chunk_count, this.#limits.maxDistinctiveChunks);
    const minAppearances = 2;
    const maxAppearances = Math.max(3, Math.ceil(totalChunks * 0.4));

    // Stream chunks one at a time to avoid loading all content into memory
    // Count document frequency (how many sections contain each word)
    const docFreq = new Map<string, number>();
    this.lastDistinctiveChunkCount = 0;

    for (const row of this.#stmtChunkContent.iterate(sourceId, this.#limits.maxDistinctiveChunks) as Iterable<{ content: string }>) {
      this.lastDistinctiveChunkCount++;
      const words = new Set(
        row.content
          .toLowerCase()
          .split(/[^\p{L}\p{N}_-]+/u)
          .filter((w) => w.length >= 3 && !STOPWORDS.has(w)),
      );
      for (const word of words) {
        docFreq.set(word, (docFreq.get(word) ?? 0) + 1);
      }
    }

    const filtered = Array.from(docFreq.entries())
      .filter(([, count]) => count >= minAppearances && count <= maxAppearances);

    // Score: IDF (rarity) + length bonus + identifier bonus (underscore/camelCase)
    const scored = filtered.map(([word, count]: [string, number]) => {
      const idf = Math.log(totalChunks / count);
      const lenBonus = Math.min(word.length / 20, 0.5);
      const hasSpecialChars = /[_]/.test(word);
      const isCamelOrLong = word.length >= 12;
      const identifierBonus = hasSpecialChars ? 1.5 : isCamelOrLong ? 0.8 : 0;
      return { word, score: idf + lenBonus + identifierBonus };
    });

    return scored
      .sort((a: { word: string; score: number }, b: { word: string; score: number }) => b.score - a.score)
      .slice(0, maxTerms)
      .map((s: { word: string; score: number }) => s.word);
  }

  // ── Stats ──

  getStats(): StoreStats {
    const row = this.#stmtStats.get() as {
      sources: number;
      chunks: number;
      codeChunks: number;
    } | undefined;

    return {
      sources: row?.sources ?? 0,
      chunks: row?.chunks ?? 0,
      codeChunks: row?.codeChunks ?? 0,
    };
  }

  // ── Cleanup ──

  /**
   * Delete sources (and their chunks) older than maxAgeDays.
   * Returns count of deleted sources.
   */
  cleanupStaleSources(maxAgeDays: number): number {
    const cleanup = this.#db.transaction((days: number) => {
      const migration = this.#db.prepare(`
        SELECT
          (SELECT value FROM store_meta WHERE key = 'indexed_bytes_backfill_cursor') AS bytes_cursor,
          (SELECT value FROM store_meta WHERE key = 'label_bytes_backfill_cursor') AS label_cursor,
          (SELECT value FROM store_meta WHERE key = 'totals_backfill_cursor') AS totals_cursor
      `).get() as { bytes_cursor: number; label_cursor: number; totals_cursor: number };
      if (migration.bytes_cursor >= 0 || migration.label_cursor >= 0 || migration.totals_cursor >= 0) {
        return { sources: 0, chunks: 0 };
      }
      const totals = this.#db.prepare(
        "SELECT indexed_bytes, chunks, sources FROM store_totals WHERE singleton = 1",
      ).get() as { indexed_bytes: number; chunks: number; sources: number };
      if (totals.indexed_bytes > this.#limits.maxIndexedBytes
        || totals.chunks > this.#limits.maxChunks
        || totals.sources > this.#limits.maxSources) {
        return { sources: 0, chunks: 0 };
      }
      const stale = this.#stmtCleanupSource.get(days) as { id: number } | undefined;
      if (!stale) return { sources: 0, chunks: 0 };
      const removed = this.#db.prepare(`
        SELECT COUNT(*) AS chunks,
          COALESCE(SUM(content_type = 'code'), 0) AS code_chunks,
          COALESCE(SUM(length(CAST(title AS BLOB)) + length(CAST(content AS BLOB))), 0) AS bytes
        FROM (SELECT title, content, content_type FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?)
      `).get(stale.id, this.#limits.maintenanceChunkBatch) as { chunks: number; code_chunks: number; bytes: number };
      const chunks = this.#stmtCleanupChunks.run(stale.id, this.#limits.maintenanceChunkBatch);
      const trigram = this.#stmtCleanupChunksTrigram.run(stale.id, this.#limits.maintenanceChunkBatch);
      if (removed.chunks > 0) {
        this.#db.prepare(`
          UPDATE sources SET
            chunk_count = MAX(0, chunk_count - ?),
            code_chunk_count = MAX(0, code_chunk_count - ?),
            indexed_bytes = MAX(length(CAST(label AS BLOB)), indexed_bytes - ?)
          WHERE id = ?
        `).run(removed.chunks, removed.code_chunks, removed.bytes, stale.id);
      }
      const source = this.#stmtCleanupSourceIfEmpty.run(stale.id, stale.id, stale.id);
      if (source.changes > 0) {
        this.#db.prepare("DELETE FROM retention_queue WHERE source_id = ?").run(stale.id);
      }
      if (chunks.changes > 0 || trigram.changes > 0) this.#rebuildVocabulary();
      return { sources: source.changes, chunks: Math.max(chunks.changes, trigram.changes) };
    });
    const info = withRetry(() => cleanup(maxAgeDays), [10, 50, 200]);
    if (info.chunks > 0) this.#fuzzyCache.clear();
    this.#checkpointWALIfNeeded();
    return info.sources;
  }

  /** Get DB file size in bytes. */
  getDBSizeBytes(): number {
    try {
      return statSync(this.#dbPath).size;
    } catch {
      return 0;
    }
  }

  /** Merge FTS5 b-tree segments for both porter and trigram indexes. */
  #optimizeFTS(): void {
    try {
      this.#db.exec("INSERT INTO chunks(chunks) VALUES('optimize')");
      this.#db.exec("INSERT INTO chunks_trigram(chunks_trigram) VALUES('optimize')");
    } catch { /* best effort — don't block indexing */ }
  }

  close(): void {
    if (this.#closed) return;
    this.#closed = true;
    try { this.#db.prepare("DELETE FROM store_owners WHERE owner_id = ?").run(this.#ownerId); } catch { /* closing damaged store */ }
    try { this.#db.close(); } catch { /* already closed */ }
    this.#releaseOwnerFence();
  }

  // ── Vocabulary Extraction ──

  #scheduleRetention(): void {
    const totals = this.#db.prepare(
      "SELECT indexed_bytes AS bytes, chunks, sources FROM store_totals WHERE singleton = 1",
    ).get() as { bytes: number; chunks: number; sources: number };
    if (totals.bytes <= this.#limits.maxIndexedBytes
      && totals.chunks <= this.#limits.maxChunks
      && totals.sources <= this.#limits.maxSources) {
      this.#db.prepare("DELETE FROM retention_queue").run();
      return;
    }
    this.#db.prepare(`
      INSERT OR IGNORE INTO retention_queue (source_id)
      SELECT id FROM sources ORDER BY id LIMIT 1
    `).run();
  }

  #runRetentionChunkBatch(): boolean {
    const queued = this.#db.prepare("SELECT source_id FROM retention_queue ORDER BY source_id LIMIT 1").get() as { source_id: number } | undefined;
    if (!queued) return false;
    const sourceId = queued.source_id;
    const removed = this.#db.prepare(`
      SELECT COUNT(*) AS chunks,
             COALESCE(SUM(content_type = 'code'), 0) AS code_chunks,
             COALESCE(SUM(length(CAST(title AS BLOB)) + length(CAST(content AS BLOB))), 0) AS bytes
      FROM (SELECT title, content, content_type FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?)
    `).get(sourceId, this.#limits.maintenanceChunkBatch) as { chunks: number; code_chunks: number; bytes: number };
    this.#db.prepare(`
      DELETE FROM chunks WHERE rowid IN (
        SELECT rowid FROM chunks WHERE source_id = ? ORDER BY rowid LIMIT ?
      )
    `).run(sourceId, this.#limits.maintenanceChunkBatch);
    this.#db.prepare(`
      DELETE FROM chunks_trigram WHERE rowid IN (
        SELECT rowid FROM chunks_trigram WHERE source_id = ? ORDER BY rowid LIMIT ?
      )
    `).run(sourceId, this.#limits.maintenanceChunkBatch);
    this.#db.prepare(`
      UPDATE sources
      SET chunk_count = MAX(0, chunk_count - ?),
          code_chunk_count = MAX(0, code_chunk_count - ?),
          indexed_bytes = MAX(0, indexed_bytes - ?)
      WHERE id = ?
    `).run(removed.chunks, removed.code_chunks, removed.bytes, sourceId);
    const remaining = this.#db.prepare("SELECT chunk_count FROM sources WHERE id = ?").get(sourceId) as { chunk_count: number } | undefined;
    if (!remaining || remaining.chunk_count === 0) {
      this.#db.prepare("DELETE FROM sources WHERE id = ?").run(sourceId);
      this.#db.prepare("DELETE FROM retention_queue WHERE source_id = ?").run(sourceId);
      if (removed.chunks > 0) {
        this.#rebuildVocabulary();
        this.#fuzzyCache.clear();
      }
    }
    return true;
  }

  #runIndexedBytesBackfillBatch(): boolean {
    const cursor = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'indexed_bytes_backfill_cursor'").get() as { value: number } | undefined;
    const maximum = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'indexed_bytes_backfill_max_rowid'").get() as { value: number } | undefined;
    if (!cursor || cursor.value < 0 || !maximum || maximum.value < 0) {
      this.lastMaintenanceBackfillRows = 0;
      return false;
    }
    const rows = this.#db.prepare(`
      SELECT rowid, source_id,
             length(CAST(title AS BLOB)) + length(CAST(content AS BLOB)) AS bytes,
             content
      FROM chunks
      WHERE rowid > ? AND rowid <= ?
      ORDER BY rowid
      LIMIT ?
    `).all(cursor.value, maximum.value, this.#limits.maintenanceChunkBatch) as Array<{
      rowid: number; source_id: number; bytes: number; content: string;
    }>;
    this.lastMaintenanceBackfillRows = rows.length;
    if (rows.length === 0) {
      this.#db.prepare("UPDATE store_meta SET value = -1 WHERE key = 'indexed_bytes_backfill_cursor'").run();
      return false;
    }
    const bytesBySource = new Map<number, number>();
    for (const row of rows) bytesBySource.set(row.source_id, (bytesBySource.get(row.source_id) ?? 0) + row.bytes);
    for (const [sourceId, bytes] of bytesBySource) {
      this.#db.prepare("UPDATE sources SET indexed_bytes = indexed_bytes + ? WHERE id = ?").run(bytes, sourceId);
    }
    this.#addVocabulary(rows.map((row) => ({ title: "", content: row.content, hasCode: false })));
    this.#db.prepare("UPDATE store_meta SET value = ? WHERE key = 'indexed_bytes_backfill_cursor'").run(rows.at(-1)!.rowid);
    return true;
  }

  #runLabelBytesBackfillBatch(): boolean {
    const cursor = this.#db.prepare(
      "SELECT value FROM store_meta WHERE key = 'label_bytes_backfill_cursor'",
    ).get() as { value: number };
    if (cursor.value < 0) return false;
    const rows = this.#db.prepare(`
      SELECT id, length(CAST(substr(label, 1, ?) AS BLOB)) AS bytes
      FROM sources WHERE id > ? ORDER BY id LIMIT ?
    `).all(MAX_SOURCE_LABEL_BYTES + 1, cursor.value, this.#limits.maintenanceChunkBatch) as Array<{ id: number; bytes: number }>;
    if (rows.length === 0) {
      this.#db.prepare(
        "UPDATE store_meta SET value = -1 WHERE key = 'label_bytes_backfill_cursor'",
      ).run();
      return false;
    }
    let vocabularyChanged = false;
    for (const row of rows) {
      if (row.bytes > MAX_SOURCE_LABEL_BYTES) {
        this.#db.prepare("DELETE FROM chunks WHERE source_id = ?").run(row.id);
        this.#db.prepare("DELETE FROM chunks_trigram WHERE source_id = ?").run(row.id);
        this.#db.prepare("DELETE FROM retention_queue WHERE source_id = ?").run(row.id);
        this.#db.prepare("DELETE FROM sources WHERE id = ?").run(row.id);
        vocabularyChanged = true;
      } else {
        this.#db.prepare("UPDATE sources SET indexed_bytes = indexed_bytes + ? WHERE id = ?")
          .run(row.bytes, row.id);
      }
    }
    if (vocabularyChanged) {
      this.#rebuildVocabulary();
      this.#fuzzyCache.clear();
    }
    this.#db.prepare(
      "UPDATE store_meta SET value = ? WHERE key = 'label_bytes_backfill_cursor'",
    ).run(rows.at(-1)!.id);
    return true;
  }

  #runTotalsBackfillBatch(): boolean {
    const cursor = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'totals_backfill_cursor'").get() as { value: number };
    if (cursor.value < 0) return false;
    const rows = this.#db.prepare(`
      SELECT id, indexed_bytes, chunk_count FROM sources
      WHERE totals_counted = 0 AND id > ? ORDER BY id LIMIT ?
    `).all(cursor.value, this.#limits.maintenanceChunkBatch) as Array<{
      id: number; indexed_bytes: number; chunk_count: number;
    }>;
    if (rows.length === 0) {
      this.#db.prepare("UPDATE store_meta SET value = -1 WHERE key = 'totals_backfill_cursor'").run();
      return false;
    }
    let bytes = 0;
    let chunks = 0;
    for (const row of rows) {
      bytes += row.indexed_bytes;
      chunks += row.chunk_count;
    }
    this.#db.prepare(`
      UPDATE store_totals SET indexed_bytes = indexed_bytes + ?, chunks = chunks + ?, sources = sources + ?
      WHERE singleton = 1
    `).run(bytes, chunks, rows.length);
    const ids = rows.map((row) => row.id);
    const placeholders = ids.map(() => "?").join(", ");
    this.#db.prepare(`UPDATE sources SET totals_counted = 1 WHERE id IN (${placeholders})`).run(...ids);
    this.#db.prepare("UPDATE store_meta SET value = ? WHERE key = 'totals_backfill_cursor'").run(rows.at(-1)!.id);
    return true;
  }

  #runMaintenanceBatch(runRetention: boolean = true): void {
    withRetry(() => this.#db.transaction(() => {
      const byteMigrationActive = this.#runIndexedBytesBackfillBatch();
      const labelMigrationActive = byteMigrationActive ? true : this.#runLabelBytesBackfillBatch();
      if (!labelMigrationActive) this.#runTotalsBackfillBatch();
      const totalsReady = this.#db.prepare("SELECT value FROM store_meta WHERE key = 'totals_backfill_cursor'").get() as { value: number };
      if (totalsReady.value < 0 && runRetention) {
        this.#scheduleRetention();
        this.#runRetentionChunkBatch();
      }
    })(), [10, 50, 200]);
  }

  #insertVocabularyWord(word: string): number {
    const info = this.#stmtInsertVocab.run(word);
    if (info.changes > 0) {
      for (const gram of vocabularyGrams(word)) this.#stmtInsertVocabGram.run(gram, word);
    }
    return info.changes;
  }

  #addVocabulary(chunks: Chunk[]): void {
    const count = this.#db.prepare("SELECT COUNT(*) AS count FROM vocabulary").get() as { count: number };
    let remaining = this.#limits.maxVocabularyTerms - count.count;
    if (remaining <= 0) return;
    const words = new Set<string>();
    for (const chunk of chunks) {
      for (const word of chunk.content.toLowerCase().split(/[^\p{L}\p{N}_-]+/u)) {
        if (word.length < 3 || word.length > this.#limits.maxFuzzyWordLength || STOPWORDS.has(word) || words.has(word)) continue;
        words.add(word);
        remaining -= this.#insertVocabularyWord(word);
        if (remaining <= 0) return;
      }
    }
  }

  #rebuildVocabulary(): void {
    this.vocabularyRebuildCount++;
    this.#db.prepare("DELETE FROM vocabulary_grams").run();
    this.#db.prepare("DELETE FROM vocabulary").run();
    const words = new Set<string>();
    for (const row of this.#stmtVocabularyContent.iterate(this.#limits.maxChunks) as Iterable<{ content: string }>) {
      for (const word of row.content.toLowerCase().split(/[^\p{L}\p{N}_-]+/u)) {
        if (word.length < 3 || word.length > this.#limits.maxFuzzyWordLength || STOPWORDS.has(word)) continue;
        words.add(word);
        if (words.size >= this.#limits.maxVocabularyTerms) break;
      }
      if (words.size >= this.#limits.maxVocabularyTerms) break;
    }
    for (const word of words) this.#insertVocabularyWord(word);
  }

  #checkpointWALIfNeeded(): void {
    try {
      const now = Date.now();
      if (now < this.#nextCheckpointAt) return;
      if (this.checkpointCount > 0 && this.#writesSinceCheckpoint < 128) return;
      const walBytes = statSync(`${this.#dbPath}-wal`).size;
      if (walBytes < this.#limits.walCheckpointBytes) return;
      const raw = this.#db.pragma("wal_checkpoint(PASSIVE)") as unknown;
      const result = (Array.isArray(raw) ? raw[0] : raw) as {
        busy?: number; log?: number; checkpointed?: number;
      } | undefined;
      this.checkpointCount++;
      this.#writesSinceCheckpoint = 0;
      const incomplete = (result?.busy ?? 0) > 0
        || (result?.checkpointed ?? 0) < (result?.log ?? 0);
      this.#nextCheckpointAt = now + (incomplete ? 5_000 : 1_000);
    } catch {
      this.#nextCheckpointAt = Date.now() + 5_000;
    }
  }

  getWALSizeBytes(): number {
    try {
      return statSync(`${this.#dbPath}-wal`).size;
    } catch {
      return 0;
    }
  }

  // ── Chunking ──

  #chunkMarkdown(text: string, maxChunkBytes: number = MAX_CHUNK_BYTES): Chunk[] {
    const chunks: Chunk[] = [];
    const lines = text.split("\n");
    const headingStack: Array<{ level: number; text: string }> = [];
    let currentContent: string[] = [];
    let currentHeading = "";

    const flush = () => {
      const joined = currentContent.join("\n").trim();
      if (joined.length === 0) return;

      const title = this.#buildTitle(headingStack, currentHeading);
      const hasCode = currentContent.some((l) => /^`{3,}/.test(l));

      // If under the cap, emit as-is (fast path — most chunks hit this)
      if (Buffer.byteLength(joined) <= maxChunkBytes) {
        chunks.push({ title, content: joined, hasCode });
        currentContent = [];
        return;
      }

      // Split oversized chunk at paragraph boundaries (double newlines)
      const paragraphs = joined.split(/\n\n+/);
      let accumulator: string[] = [];
      let partIndex = 1;

      const flushAccumulator = () => {
        if (accumulator.length === 0) return;
        const part = accumulator.join("\n\n").trim();
        if (part.length === 0) return;
        const partTitle = paragraphs.length > 1 ? `${title} (${partIndex})` : title;
        partIndex++;
        chunks.push({
          title: partTitle,
          content: part,
          hasCode: part.includes("```"),
        });
        accumulator = [];
      };

      for (const para of paragraphs) {
        accumulator.push(para);
        const candidate = accumulator.join("\n\n");
        if (Buffer.byteLength(candidate) > maxChunkBytes && accumulator.length > 1) {
          accumulator.pop();
          flushAccumulator();
          accumulator = [para];
        }
      }
      flushAccumulator();

      currentContent = [];
    };

    let i = 0;
    while (i < lines.length) {
      const line = lines[i];

      // Horizontal rule separator (Context7 uses long dashes)
      if (/^[-_*]{3,}\s*$/.test(line)) {
        flush();
        i++;
        continue;
      }

      // Heading (H1-H4)
      const headingMatch = line.match(/^(#{1,4})\s+(.+)$/);
      if (headingMatch) {
        flush();

        const level = headingMatch[1].length;
        const heading = headingMatch[2].trim();

        // Pop deeper levels from stack
        while (
          headingStack.length > 0 &&
          headingStack[headingStack.length - 1].level >= level
        ) {
          headingStack.pop();
        }
        headingStack.push({ level, text: heading });
        currentHeading = heading;

        currentContent.push(line);
        i++;
        continue;
      }

      // Code block — collect entire block as a unit
      const codeMatch = line.match(/^(`{3,})(.*)?$/);
      if (codeMatch) {
        const fence = codeMatch[1];
        const codeLines: string[] = [line];
        i++;

        while (i < lines.length) {
          codeLines.push(lines[i]);
          if (lines[i].startsWith(fence) && lines[i].trim() === fence) {
            i++;
            break;
          }
          i++;
        }

        currentContent.push(...codeLines);
        continue;
      }

      // Regular line
      currentContent.push(line);
      i++;
    }

    // Flush remaining content
    flush();

    return chunks;
  }

  /**
   * Split a single oversized plain-text chunk into byte-capped sub-chunks
   * by accumulating lines until the byte count would exceed maxChunkBytes.
   * Falls back to byte-accurate splitting for extremely long single lines.
   */
  #splitOversizedPlainChunk(
    lines: string[],
    titlePrefix: string,
    maxChunkBytes: number,
  ): Array<{ title: string; content: string }> {
    const subChunks: Array<{ title: string; content: string }> = [];
    let accumulator: string[] = [];
    let partIndex = 1;

    const flushAccumulator = () => {
      if (accumulator.length === 0) return;
      const content = accumulator.join("\n");
      const partTitle = partIndex === 1 ? titlePrefix : `${titlePrefix} (${partIndex})`;
      subChunks.push({ title: partTitle, content });
      partIndex++;
      accumulator = [];
    };

    for (const line of lines) {
      // If a single line itself exceeds the cap (even as first line),
      // split it by character before accumulating
      if (Buffer.byteLength(line) > maxChunkBytes) {
        flushAccumulator();
        const encodedLine = Buffer.from(line);
        let byteOffset = 0;
        let linePart = 1;
        while (byteOffset < encodedLine.length) {
          let byteEnd = Math.min(byteOffset + maxChunkBytes, encodedLine.length);
          while (byteEnd > byteOffset
            && byteEnd < encodedLine.length
            && (encodedLine[byteEnd] & 0xc0) === 0x80) byteEnd--;
          if (byteEnd === byteOffset) {
            byteEnd = Math.min(byteOffset + maxChunkBytes, encodedLine.length);
            while (byteEnd < encodedLine.length && (encodedLine[byteEnd] & 0xc0) === 0x80) byteEnd++;
          }
          let slice = encodedLine.subarray(byteOffset, byteEnd).toString("utf8");
          if (byteEnd < encodedLine.length) {
            const lastSpace = slice.lastIndexOf(" ");
            const lastNewline = slice.lastIndexOf("\n");
            const breakPoint = Math.max(lastSpace, lastNewline);
            if (breakPoint > slice.length * WHITESPACE_BREAK_RATIO) {
              slice = slice.slice(0, breakPoint);
              byteEnd = byteOffset + Buffer.byteLength(slice);
            }
          }
          const linePartTitle = partIndex === 1 && linePart === 1
            ? titlePrefix
            : `${titlePrefix} (${partIndex}.${linePart})`;
          subChunks.push({ title: linePartTitle, content: slice });
          byteOffset = byteEnd;
          linePart++;
          partIndex++;
        }
        continue;
      }

      const candidate = accumulator.length > 0
        ? accumulator.join("\n") + "\n" + line
        : line;

      // If adding this line would exceed the cap, flush accumulator first
      if (Buffer.byteLength(candidate) > maxChunkBytes && accumulator.length > 0) {
        flushAccumulator();
      }
      accumulator.push(line);
    }
    flushAccumulator();
    return subChunks;
  }

  #chunkPlainText(
    text: string,
    linesPerChunk: number,
    maxChunkBytes: number = MAX_CHUNK_BYTES,
  ): Array<{ title: string; content: string }> {
    // Try blank-line splitting first for naturally-sectioned output
    const sections = text.split(/\n\s*\n/);
    if (
      sections.length >= MIN_BLANK_LINE_SECTIONS &&
      sections.length <= MAX_BLANK_LINE_SECTIONS &&
      sections.every((s) => Buffer.byteLength(s) < BLANK_SECTION_STRATEGY_MAX_BYTES)
    ) {
      return sections.flatMap((section, i) => {
        const trimmed = section.trim();
        if (trimmed.length === 0) return [];
        const title = trimmed.split("\n")[0].slice(0, CHUNK_TITLE_MAX_CHARS) || `Section ${i + 1}`;
        // A section may pass the strategy guard yet still exceed the byte cap
        // (4097–4999B band): sub-split it so no stored chunk breaks the cap.
        if (Buffer.byteLength(trimmed) <= maxChunkBytes) {
          return [{ title, content: trimmed }];
        }
        return this.#splitOversizedPlainChunk(trimmed.split("\n"), title, maxChunkBytes);
      });
    }

    const lines = text.split("\n");

    // Small enough for a single chunk — but still enforce byte cap
    if (lines.length <= linesPerChunk) {
      if (Buffer.byteLength(text) <= maxChunkBytes) {
        return [{ title: "Output", content: text }];
      }
      return this.#splitOversizedPlainChunk(lines, "Output", maxChunkBytes);
    }

    // Fixed-size line groups with 2-line overlap
    const chunks: Array<{ title: string; content: string }> = [];
    const overlap = 2;
    const step = Math.max(linesPerChunk - overlap, 1);

    for (let i = 0; i < lines.length; i += step) {
      const slice = lines.slice(i, i + linesPerChunk);
      if (slice.length === 0) break;
      const startLine = i + 1;
      const endLine = Math.min(i + slice.length, lines.length);
      const firstLine = slice[0]?.trim().slice(0, CHUNK_TITLE_MAX_CHARS);
      const joined = slice.join("\n");

      // Enforce byte cap: sub-split oversized line-group chunks
      if (Buffer.byteLength(joined) <= maxChunkBytes) {
        chunks.push({
          title: firstLine || `Lines ${startLine}-${endLine}`,
          content: joined,
        });
      } else {
        const subChunks = this.#splitOversizedPlainChunk(
          slice,
          firstLine || `Lines ${startLine}-${endLine}`,
          maxChunkBytes,
        );
        chunks.push(...subChunks);
      }
    }

    return chunks;
  }

  #walkJSON(
    value: unknown,
    path: string[],
    chunks: Chunk[],
    maxChunkBytes: number,
  ): void {
    let title = "";
    for (const segment of path) {
      const separator = title ? " > " : "";
      const remaining = CHUNK_TITLE_MAX_CHARS - title.length - separator.length;
      if (remaining <= 0) break;
      title += separator + segment.slice(0, remaining);
    }
    if (!title) title = "(root)";
    const isNestedObject = typeof value === "object"
      && value !== null
      && !Array.isArray(value)
      && Object.values(value).some((child) => typeof child === "object" && child !== null);
    if (isNestedObject) {
      for (const [key, child] of Object.entries(value as Record<string, unknown>)) {
        this.#walkJSON(child, [...path, key], chunks, maxChunkBytes);
      }
      return;
    }
    const serialized = JSON.stringify(value, null, 2);

    if (Buffer.byteLength(serialized) <= maxChunkBytes) {
      chunks.push({ title, content: serialized, hasCode: true });
      return;
    }

    // Object — recurse into each key
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      const entries = Object.entries(value);
      if (entries.length > 0) {
        for (const [key, val] of entries) {
          this.#walkJSON(val, [...path, key], chunks, maxChunkBytes);
        }
        return;
      }
      // Empty object — emit as-is
      chunks.push({ title, content: serialized, hasCode: true });
      return;
    }

    // Array — batch by size with identity-field-aware titles
    if (Array.isArray(value)) {
      this.#chunkJSONArray(value, path, chunks, maxChunkBytes);
      return;
    }

    // Primitive that exceeds maxChunkBytes (e.g., very long string)
    chunks.push({ title, content: serialized, hasCode: false });
  }

  /**
   * Scan the first element of an array of objects for a recognizable
   * identity field. Returns the field name or null.
   */
  #findIdentityField(arr: unknown[]): string | null {
    if (arr.length === 0) return null;
    const first = arr[0];
    if (typeof first !== "object" || first === null || Array.isArray(first)) return null;

    const candidates = ["id", "name", "title", "path", "slug", "key", "label"];
    const obj = first as Record<string, unknown>;
    for (const field of candidates) {
      if (field in obj && (typeof obj[field] === "string" || typeof obj[field] === "number")) {
        return field;
      }
    }
    return null;
  }

  #jsonBatchTitle(
    prefix: string,
    startIdx: number,
    endIdx: number,
    batch: unknown[],
    identityField: string | null,
  ): string {
    const sep = prefix ? `${prefix} > ` : "";

    if (!identityField) {
      return startIdx === endIdx
        ? `${sep}[${startIdx}]`
        : `${sep}[${startIdx}-${endIdx}]`;
    }

    const getId = (item: unknown) =>
      String((item as Record<string, unknown>)[identityField]);

    if (batch.length === 1) {
      return `${sep}${getId(batch[0])}`;
    }
    if (batch.length <= 3) {
      return sep + batch.map(getId).join(", ");
    }
    return `${sep}${getId(batch[0])}\u2026${getId(batch[batch.length - 1])}`;
  }

  #chunkJSONArray(
    arr: unknown[],
    path: string[],
    chunks: Chunk[],
    maxChunkBytes: number,
  ): void {
    const prefix = path.length > 0 ? path.join(" > ") : "(root)";
    const identityField = this.#findIdentityField(arr);

    let batch: unknown[] = [];
    let serializedBatch: string[] = [];
    let batchBytes = 4;
    let batchStart = 0;

    const flushBatch = (batchEnd: number) => {
      if (batch.length === 0) return;
      const title = this.#jsonBatchTitle(prefix, batchStart, batchEnd, batch, identityField);
      chunks.push({
        title,
        content: `[\n${serializedBatch.join(",\n")}\n]`,
        hasCode: true,
      });
    };

    for (let i = 0; i < arr.length; i++) {
      const serialized = JSON.stringify(arr[i], null, 2);
      const serializedBytes = Buffer.byteLength(serialized);
      const separatorBytes = batch.length > 0 ? 2 : 0;
      if (batch.length > 0 && batchBytes + separatorBytes + serializedBytes > maxChunkBytes) {
        flushBatch(i - 1);
        batch = [];
        serializedBatch = [];
        batchBytes = 4;
        batchStart = i;
      }
      batch.push(arr[i]);
      serializedBatch.push(serialized);
      batchBytes += (batch.length > 1 ? 2 : 0) + serializedBytes;
    }

    flushBatch(batchStart + batch.length - 1);
  }

  #buildTitle(
    headingStack: Array<{ level: number; text: string }>,
    currentHeading: string,
  ): string {
    if (headingStack.length === 0) {
      return currentHeading || "Untitled";
    }
    return headingStack.map((h) => h.text).join(" > ");
  }
}
