/**
 * Shared localhost HTTP daemon for QuietContext.
 *
 * One process serves every local agent session over Streamable HTTP instead of
 * one resident stdio child per session. Stateless transport
 * (sessionIdGenerator: undefined): each POST builds a throwaway McpServer from
 * REGISTERED_CTX_TOOLS, so no protocol session state exists to leak between
 * clients. Per-request context (working root, session id) travels in headers
 * and is scoped via withProjectDirOverride (AsyncLocalStorage) — never via
 * process env or globals.
 *
 * Client wiring (plugin manifest):
 *   headers.X-QuietContext-Root = "${PWD}"   — expanded by the host per session
 *   headersHelper daemon-headers.mjs         — reads the bearer token file
 */
import { createServer, type IncomingMessage, type ServerResponse } from "node:http";
import { randomBytes, timingSafeEqual } from "node:crypto";
import { closeSync, mkdirSync, openSync, readFileSync, statSync, writeSync } from "node:fs";
import { dirname, isAbsolute, join } from "node:path";
import { homedir } from "node:os";
import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StreamableHTTPServerTransport } from "@modelcontextprotocol/sdk/server/streamableHttp.js";
import {
  ListPromptsRequestSchema,
  ListResourcesRequestSchema,
  ListResourceTemplatesRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import {
  REGISTERED_CTX_TOOLS,
  VERSION,
  installStrictClientSchemaCompat,
  resolveSessionIdFromSessionDB,
  setDaemonMode,
  withProjectDirOverride,
} from "./server.js";
export { releaseProcessResources } from "./server.js";

export const DEFAULT_DAEMON_PORT = 48619;
export const ROOT_HEADER = "x-quietcontext-root";

export function defaultTokenFilePath(): string {
  return join(homedir(), ".local", "state", "quietcontext", "daemon.token");
}

/**
 * Read the shared bearer token, creating it atomically (0600, O_EXCL) when
 * absent. daemon-headers.mjs mirrors this logic on the client side so either
 * end may run first; O_EXCL makes the race safe.
 */
export function ensureDaemonToken(tokenFile: string = defaultTokenFilePath()): string {
  const read = (): string | null => {
    try {
      const raw = readFileSync(tokenFile, "utf8").trim();
      return raw.length >= 32 ? raw : null;
    } catch {
      return null;
    }
  };
  const existing = read();
  if (existing) return existing;
  mkdirSync(dirname(tokenFile), { recursive: true, mode: 0o700 });
  const fresh = randomBytes(32).toString("hex");
  try {
    const fd = openSync(tokenFile, "wx", 0o600);
    try {
      writeSync(fd, fresh + "\n");
    } finally {
      closeSync(fd);
    }
    return fresh;
  } catch (err) {
    // Lost the creation race — the other writer's token is authoritative.
    const raced = read();
    if (raced) return raced;
    throw err;
  }
}

function tokenMatches(header: string | undefined, token: string): boolean {
  if (!header) return false;
  const m = /^Bearer\s+(\S+)$/i.exec(header);
  if (!m) return false;
  const presented = Buffer.from(m[1]);
  const expected = Buffer.from(token);
  if (presented.length !== expected.length) return false;
  return timingSafeEqual(presented, expected);
}

function sendJson(res: ServerResponse, status: number, body: Record<string, unknown>): void {
  const payload = JSON.stringify(body);
  res.writeHead(status, { "content-type": "application/json" });
  res.end(payload);
}

function jsonRpcError(res: ServerResponse, status: number, code: number, message: string): void {
  sendJson(res, status, { jsonrpc: "2.0", error: { code, message }, id: null });
}

const MAX_BODY_BYTES = 32 * 1024 * 1024;

async function readBody(req: IncomingMessage): Promise<string | null> {
  let size = 0;
  const chunks: Buffer[] = [];
  for await (const chunk of req) {
    const buf = chunk as Buffer;
    size += buf.length;
    if (size > MAX_BODY_BYTES) return null;
    chunks.push(buf);
  }
  return Buffer.concat(chunks).toString("utf8");
}

function containsToolCall(parsed: unknown): boolean {
  const msgs = Array.isArray(parsed) ? parsed : [parsed];
  return msgs.some(
    (m) => m !== null && typeof m === "object" && (m as { method?: unknown }).method === "tools/call",
  );
}

/** Absolute + existing directory, or null. */
function validateRoot(raw: string | undefined): string | null {
  if (!raw || !isAbsolute(raw)) return null;
  try {
    return statSync(raw).isDirectory() ? raw : null;
  } catch {
    return null;
  }
}

function buildRequestServer(projectDir: string | null): McpServer {
  const mcp = new McpServer({ name: "quietcontext", version: VERSION });
  mcp.server.registerCapabilities({
    prompts: { listChanged: false },
    resources: { listChanged: false },
  });
  mcp.server.setRequestHandler(ListPromptsRequestSchema, async () => ({ prompts: [] }));
  mcp.server.setRequestHandler(ListResourcesRequestSchema, async () => ({ resources: [] }));
  mcp.server.setRequestHandler(ListResourceTemplatesRequestSchema, async () => ({
    resourceTemplates: [],
  }));
  for (const tool of REGISTERED_CTX_TOOLS) {
    const scopedHandler = async (args: Record<string, unknown>) => {
      if (!projectDir) {
        throw new Error(
          `quietcontext daemon: missing/invalid ${ROOT_HEADER} header — the client must send its working root`,
        );
      }
      const sessionId = resolveSessionIdFromSessionDB({ projectDir, bypassCache: true });
      return withProjectDirOverride({ projectDir, sessionId }, async () => tool.handler(args));
    };
    (mcp.registerTool as unknown as (...a: unknown[]) => unknown)(
      tool.name,
      tool.config,
      scopedHandler,
    );
  }
  // Reuse the same schema-shaping pass the stdio path installs on `server`
  // (module-init, src/server.ts) so both transports emit byte-identical
  // tools/list schemas — same strict-client sanitizer, one implementation.
  installStrictClientSchemaCompat(mcp);
  return mcp;
}

export interface HttpDaemonOptions {
  port?: number;
  host?: string;
  tokenFile?: string;
}

export interface HttpDaemonHandle {
  port: number;
  close(): Promise<void>;
}

export async function startHttpDaemon(opts: HttpDaemonOptions = {}): Promise<HttpDaemonHandle> {
  const host = opts.host ?? "127.0.0.1";
  const port = opts.port ?? DEFAULT_DAEMON_PORT;
  const token = ensureDaemonToken(opts.tokenFile);
  // Item 3 — this process serves many roots for its whole (long) lifetime,
  // unlike the stdio child (one root, one session). Only here does idle-store
  // eviction make sense.
  setDaemonMode(true);

  const httpServer = createServer(async (req, res) => {
    try {
      const url = req.url ?? "/";
      if (req.method === "GET" && url === "/healthz") {
        sendJson(res, 200, { ok: true, name: "quietcontext", version: VERSION, pid: process.pid });
        return;
      }
      if (url !== "/mcp") {
        sendJson(res, 404, { error: "not found" });
        return;
      }
      if (!tokenMatches(req.headers.authorization, token)) {
        jsonRpcError(res, 401, -32000, "unauthorized");
        return;
      }
      if (req.method !== "POST") {
        // Stateless mode: no SSE notification stream, no session to delete.
        res.writeHead(405, { allow: "POST" }).end();
        return;
      }
      const body = await readBody(req);
      if (body === null) {
        jsonRpcError(res, 413, -32000, "request body too large");
        return;
      }
      let parsed: unknown;
      try {
        parsed = body.length > 0 ? JSON.parse(body) : undefined;
      } catch {
        jsonRpcError(res, 400, -32700, "parse error");
        return;
      }
      const root = validateRoot(req.headers[ROOT_HEADER] as string | undefined);
      if (containsToolCall(parsed) && !root) {
        jsonRpcError(
          res,
          400,
          -32000,
          `missing or invalid ${ROOT_HEADER} header: expected the caller's absolute working directory`,
        );
        return;
      }
      const mcp = buildRequestServer(root);
      const transport = new StreamableHTTPServerTransport({ sessionIdGenerator: undefined });
      res.on("close", () => {
        void transport.close();
        void mcp.close();
      });
      await mcp.connect(transport);
      await transport.handleRequest(req, res, parsed);
    } catch (err) {
      console.error("[quietcontext-daemon] request failed:", err);
      if (!res.headersSent) jsonRpcError(res, 500, -32603, "internal error");
      else res.end();
    }
  });

  await new Promise<void>((resolvePromise, reject) => {
    httpServer.once("error", reject);
    httpServer.listen(port, host, () => resolvePromise());
  });
  const bound = httpServer.address();
  const boundPort = typeof bound === "object" && bound !== null ? bound.port : port;
  console.error(`[quietcontext-daemon] v${VERSION} listening on http://${host}:${boundPort}/mcp`);
  return {
    port: boundPort,
    close: () =>
      new Promise<void>((resolvePromise, reject) => {
        setDaemonMode(false);
        httpServer.close((err) => (err ? reject(err) : resolvePromise()));
      }),
  };
}
