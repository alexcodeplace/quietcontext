import {
  existsSync,
  linkSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { basename, dirname, join } from "node:path";
import { randomBytes } from "node:crypto";

interface ProcessIdentity {
  pid: number;
  startToken?: string;
}

function fenceDir(dbPath: string): string {
  return `${dbPath}.access`;
}

function migrationPath(dbPath: string): string {
  return join(fenceDir(dbPath), "migration");
}

function processStartToken(pid: number): string | undefined {
  if (process.platform !== "linux") return undefined;
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
    const fields = stat.slice(stat.lastIndexOf(")") + 1).trim().split(/\s+/);
    return fields[19] || undefined;
  } catch {
    return undefined;
  }
}

function currentProcessIdentity(): ProcessIdentity {
  return { pid: process.pid, startToken: processStartToken(process.pid) };
}

function parseIdentity(raw: string, fallbackPid?: number): ProcessIdentity | undefined {
  try {
    const parsed = JSON.parse(raw) as Partial<ProcessIdentity>;
    if (Number.isInteger(parsed.pid) && parsed.pid! > 0) {
      return {
        pid: parsed.pid!,
        startToken: typeof parsed.startToken === "string" ? parsed.startToken : undefined,
      };
    }
  } catch { /* legacy or incomplete record */ }
  const legacyPid = Number(raw);
  if (Number.isInteger(legacyPid) && legacyPid > 0) return { pid: legacyPid };
  if (Number.isInteger(fallbackPid) && fallbackPid! > 0) return { pid: fallbackPid! };
  return undefined;
}

function processIdentityAlive(identity: ProcessIdentity): boolean {
  try {
    process.kill(identity.pid, 0);
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EPERM") return true;
    return false;
  }
  if (!identity.startToken) return true;
  const currentStart = processStartToken(identity.pid);
  return currentStart === undefined || currentStart === identity.startToken;
}

function clearStaleMigration(dbPath: string): void {
  const path = migrationPath(dbPath);
  try {
    const identity = parseIdentity(readFileSync(path, "utf8"));
    if (!identity || processIdentityAlive(identity)) return;
    unlinkSync(path);
  } catch { /* absent or concurrently removed */ }
}

function publishMigrationMarker(path: string): void {
  const dir = dirname(path);
  const temporary = join(dir, `.migration-${process.pid}-${randomBytes(12).toString("hex")}`);
  writeFileSync(temporary, JSON.stringify(currentProcessIdentity()), { flag: "wx", mode: 0o600 });
  try {
    linkSync(temporary, path);
  } finally {
    try { unlinkSync(temporary); } catch { /* already removed */ }
  }
}

export function acquireContentStoreOwnerFence(dbPath: string): () => void {
  const dir = fenceDir(dbPath);
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  clearStaleMigration(dbPath);
  const ownerPath = join(dir, `owner-${process.pid}-${randomBytes(12).toString("hex")}`);
  writeFileSync(ownerPath, JSON.stringify(currentProcessIdentity()), { flag: "wx", mode: 0o600 });
  if (existsSync(migrationPath(dbPath))) {
    unlinkSync(ownerPath);
    throw new Error(`Content store migration is active for ${dbPath}`);
  }
  return () => {
    try { unlinkSync(ownerPath); } catch { /* already released */ }
  };
}

export function acquireContentStoreMigrationFence(dbPath: string): () => void {
  const dir = fenceDir(dbPath);
  mkdirSync(dir, { recursive: true, mode: 0o700 });
  clearStaleMigration(dbPath);
  const path = migrationPath(dbPath);
  publishMigrationMarker(path);
  try {
    for (const file of readdirSync(dir)) {
      if (!file.startsWith("owner-")) continue;
      const ownerPath = join(dir, file);
      const fallbackPid = Number(file.split("-")[1]);
      let identity: ProcessIdentity | undefined;
      try {
        identity = parseIdentity(readFileSync(ownerPath, "utf8"), fallbackPid);
      } catch { /* concurrently removed */ }
      if (!identity || processIdentityAlive(identity)) {
        throw new Error(`Content store ${basename(dbPath)} has a live or unverifiable owner`);
      }
      try { unlinkSync(ownerPath); } catch { /* concurrently removed */ }
    }
  } catch (error) {
    try { unlinkSync(path); } catch { /* already released */ }
    throw error;
  }
  return () => {
    try { unlinkSync(path); } catch { /* already released */ }
  };
}
