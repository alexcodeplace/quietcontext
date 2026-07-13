/**
 * Agent-facing context-mode routing guidance.
 */

import { createToolNamer } from "./core/tool-naming.mjs";

export function createRoutingBlock(t, options = {}) {
  const { includeCommands = true, toolSearchBootstrap = false } = options;
  return `<context_mode>
  Use ${t("ctx_execute")} or ${t("ctx_execute_file")} to process large data; raw bytes stay in the sandbox and only derived output enters context.
  Use ${t("ctx_batch_execute")} for multiple commands and ${t("ctx_search")} for indexed content or session memory.
  Use native shell tools for short observations and mutations. Native Write/Edit handles file changes; ctx tools do not persist edits.
  Web docs: use ${t("ctx_fetch_and_index")} then ${t("ctx_search")}.
  Artifacts go to files; return the file path + 1-line description.
${toolSearchBootstrap ? `  If a ctx_* schema is unavailable, load it once with ToolSearch before retrying.
` : ""}${includeCommands ? `  maintenance: ${t("ctx_stats")}, ${t("ctx_doctor")}, ${t("ctx_purge")} only on explicit request.
` : ""}</context_mode>`;
}

export function createReadGuidance(t) {
  return `<context_guidance>Use native Read when editing; use ${t("ctx_execute_file")} for analysis so raw bytes stay in the sandbox.</context_guidance>`;
}

export function createGrepGuidance(t) {
  return `<context_guidance>Use native grep for short spot-checks; use ${t("ctx_execute")} for counting, filtering, or aggregation.</context_guidance>`;
}

export function createBashGuidance(t) {
  return `<context_guidance>Use ${t("ctx_batch_execute")} or ${t("ctx_execute")} for processing; use native shell for short observations or mutations.</context_guidance>`;
}

export function createExternalMcpGuidance(t) {
  return `<context_guidance>External MCP tools: derive large results with ${t("ctx_execute")}; for docs, use ${t("ctx_fetch_and_index")} then ${t("ctx_search")}.</context_guidance>`;
}

const _t = createToolNamer("claude-code");
export const ROUTING_BLOCK = createRoutingBlock(_t);
export const READ_GUIDANCE = createReadGuidance(_t);
export const GREP_GUIDANCE = createGrepGuidance(_t);
export const BASH_GUIDANCE = createBashGuidance(_t);
export const EXTERNAL_MCP_GUIDANCE = createExternalMcpGuidance(_t);
