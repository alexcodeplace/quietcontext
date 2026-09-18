import { appendFileSync, existsSync, mkdirSync, readFileSync, realpathSync, renameSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, parse, resolve } from "node:path";

const DEFAULT_TIMEOUT_MS = 650;
const MAX_CONTEXT_BYTES = 4 * 1024;
const MAX_TELEMETRY_BYTES = 1024 * 1024;
const DEFAULT_DAEMON_PORT = 48619;

const STRUCTURAL_TERMS = [
  "how does","how do","why does","why is","architecture","dependency","dependencies","caller","callers","callee","callees","call flow","data flow","trace","where is","who calls","what calls","impact","blast radius","implementation","implemented","module","service","handler","controller","state machine","bug","fix","failure","fails","error","debug","refactor","implement","add feature","find",
  "איך","כיצד","איפה","מי קורא","תלויות","ארכיטקטורה","זרימה","מימוש","באג","תקן","ריפקטור",
  "comment","dépend","appel","flux","implément","corrige",
  "cómo","arquitectura","dependencia","llama","flujo","implement","corrige",
  "wie","architektur","abhängig","aufruf","fluss","implement","fehler",
  "come","architettura","dipenden","chiama","flusso","implement",
  "как","архитект","зависим","вызывает","поток","реализ","ошибк",
  "如何","怎么","架构","依赖","调用","流程","实现","修复",
  "どのよう","アーキテクチャ","依存","呼び出","フロー","実装","修正",
  "어떻게","아키텍처","의존","호출","흐름","구현","수정",
  "كيف","بنية","اعتماد","استدع","تدفق","تنفيذ","إصلاح",
  "อย่างไร","สถาปัตย","พึ่งพ","เรียก","โฟลว์","แก้ไข",
];

const CODE_SHAPE = /(?:`[^`]+`|\b[A-Za-z_$][\w$]*(?:[.:/][A-Za-z_$][\w$]*)+\b|\b[A-Za-z_$][a-z0-9_$]*[A-Z][A-Za-z0-9_$]*\b|\b[A-Za-z_$][\w$]*_[A-Za-z0-9_$]+\b|\b[\w.-]+\.(?:ts|tsx|js|jsx|mjs|cjs|py|rs|go|java|kt|php|rb|cs|cpp|c|h|vue|svelte|astro)\b|\b[A-Za-z_$][\w$]*\(\))/;

export function isSystemPromptMessage(prompt) {
  const text = String(prompt ?? "").trim();
  return text.startsWith("<task-notification>")
    || text.startsWith("<system-reminder>")
    || text.startsWith("<context_guidance>")
    || text.startsWith("<tool-result>");
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function matchesStructuralTerm(lower, term) {
  if (!/^[a-z0-9 -]+$/.test(term)) return lower.includes(term);
  const pattern = escapeRegExp(term).replace(/ +/g, "\\s+");
  return new RegExp("(^|[^a-z0-9])" + pattern + "([^a-z0-9]|$)", "i").test(lower);
}

export function shouldFrontloadPrompt(prompt) {
  const text = String(prompt ?? "").trim();
  if (text.length < 4 || isSystemPromptMessage(text) || text.startsWith("/")) return false;
  if (CODE_SHAPE.test(text)) return true;
  const lower = text.toLocaleLowerCase();
  return STRUCTURAL_TERMS.some((term) => matchesStructuralTerm(lower, term));
}

function projectMarker(dir) {
  return [".git","package.json","Cargo.toml","go.mod","pyproject.toml","pom.xml","build.gradle","build.gradle.kts"]
    .some((name) => existsSync(resolve(dir, name)));
}

export function resolveSafeFrontloadRoot(inputDir) {
  try {
    let current = realpathSync(resolve(String(inputDir ?? process.cwd())));
    const home = realpathSync(homedir());
    const root = parse(current).root;
    if (current === home || current === root) return null;
    for (let i = 0; i < 10; i++) {
      if (projectMarker(current)) return current;
      const parent = dirname(current);
      if (parent === current || parent === home || parent === root) break;
      current = parent;
    }
  } catch {}
  return null;
}

function capUtf8(value, maxBytes) {
  if (Buffer.byteLength(value) <= maxBytes) return value;
  let end = Math.min(value.length, maxBytes);
  while (end > 0 && Buffer.byteLength(value.slice(0, end)) > maxBytes) end--;
  return value.slice(0, end).replace(/\s+$/u, "");
}

function telemetryPath() {
  return process.env.QUIET_CONTEXT_ADOPTION_TELEMETRY_PATH
    || resolve(homedir(), ".local", "state", "quietcontext", "adoption.jsonl");
}

export function recordFrontloadTelemetry(outcome, latencyMs = 0, bytes = 0) {
  if (process.env.QUIET_CONTEXT_ADOPTION_TELEMETRY_DISABLE === "1") return;
  if (!["injected","no-match","timeout","error"].includes(outcome)) return;
  try {
    const path = telemetryPath();
    mkdirSync(dirname(path), { recursive: true, mode: 0o700 });
    if (existsSync(path) && statSync(path).size > MAX_TELEMETRY_BYTES) {
      try { renameSync(path, path + ".1"); } catch {}
    }
    appendFileSync(path, JSON.stringify({
      ts: new Date().toISOString(), event: "prompt-frontload", outcome,
      latency_ms: Math.round(latencyMs), bytes,
    }) + "\n", { mode: 0o600 });
  } catch {}
}

function tokenFilePath() {
  return process.env.QUIET_CONTEXT_DAEMON_TOKEN_FILE
    || resolve(homedir(), ".local", "state", "quietcontext", "daemon.token");
}

export async function daemonExplore(prompt, root, timeoutMs) {
  if (process.env.QUIET_CONTEXT_ADOPTION_TEST_MODE === "1" && process.env.QUIET_CONTEXT_FRONTLOAD_TEST_RESPONSE) {
    return process.env.QUIET_CONTEXT_FRONTLOAD_TEST_RESPONSE;
  }
  const token = readFileSync(tokenFilePath(), "utf8").trim();
  if (!token) return "";
  const port = Number(process.env.QUIET_CONTEXT_DAEMON_PORT || DEFAULT_DAEMON_PORT);
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  timer.unref?.();
  try {
    const response = await fetch("http://127.0.0.1:" + port + "/mcp", {
      method: "POST",
      signal: controller.signal,
      headers: {
        "content-type": "application/json",
        "accept": "application/json, text/event-stream",
        "authorization": "Bearer " + token,
        "x-quietcontext-root": root,
      },
      body: JSON.stringify({
        jsonrpc: "2.0", id: 1, method: "tools/call",
        params: { name: "repo", arguments: { action: "frontload", target: String(prompt).slice(0, 1200) } },
      }),
    });
    if (!response.ok) return "";
    const wire = await response.text();
    const data = wire.split("\n").find((line) => line.startsWith("data: "));
    const payload = JSON.parse(data ? data.slice(6) : wire);
    if (payload.error || payload.result?.isError) return "";
    return (payload.result?.content ?? [])
      .filter((item) => item?.type === "text" && typeof item.text === "string")
      .map((item) => item.text)
      .join("\n")
      .trim();
  } finally {
    clearTimeout(timer);
  }
}

export async function frontloadPromptContext(prompt, projectDir, options = {}) {
  const started = Date.now();
  if (process.env.QUIET_CONTEXT_NO_PROMPT_HOOK === "1" || process.env.QUIET_CONTEXT_FRONTLOAD_DISABLE === "1") return "";
  if (!shouldFrontloadPrompt(prompt)) return "";
  const root = resolveSafeFrontloadRoot(projectDir);
  if (!root) return "";
  const timeoutMs = Math.max(100, Math.min(Number(options.timeoutMs ?? process.env.QUIET_CONTEXT_FRONTLOAD_TIMEOUT_MS ?? DEFAULT_TIMEOUT_MS), 2000));
  try {
    const requestExplore = options.requestExplore ?? daemonExplore;
    const body = String(await requestExplore(prompt, root, timeoutMs) ?? "").trim();
    if (!body || body.includes("No high-confidence symbol/file match.")) {
      recordFrontloadTelemetry("no-match", Date.now() - started, 0);
      return "";
    }
    const wrapped = "<qc_context source=\"prompt-frontload\" note=\"Repository context already inspected; prefer QC repo explore over exploratory Read/Grep unless incomplete or stale.\">\n"
      + body + "\n</qc_context>";
    const context = capUtf8(wrapped, MAX_CONTEXT_BYTES);
    recordFrontloadTelemetry("injected", Date.now() - started, Buffer.byteLength(context));
    return context;
  } catch (error) {
    const outcome = error?.name === "AbortError" ? "timeout" : "error";
    recordFrontloadTelemetry(outcome, Date.now() - started, 0);
    return "";
  }
}
