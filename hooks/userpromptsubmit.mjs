#!/usr/bin/env node
/**
 * UserPromptSubmit hook for QuietContext session continuity + semantic front-load.
 *
 * Responsibilities are independent and fail-open:
 * - persist the genuine user prompt/events for session continuity
 * - for structural code prompts, inject bounded QC repository context before reasoning
 */

import { runHook } from "./run-hook.mjs";

await runHook(async () => {
  const {
    readStdin, parseStdin, getSessionId, getSessionDBPath, getInputProjectDir,
  } = await import("./session-helpers.mjs");
  const { createSessionLoaders, attributeAndInsertEvents } = await import("./session-loaders.mjs");
  const { dirname } = await import("node:path");
  const { fileURLToPath } = await import("node:url");

  const HOOK_DIR = dirname(fileURLToPath(import.meta.url));
  const { loadSessionDB, loadExtract, loadProjectAttribution } = createSessionLoaders(HOOK_DIR);
  let additionalContext = "";

  try {
    const raw = await readStdin();
    const input = parseStdin(raw);
    const projectDir = getInputProjectDir(input);
    const prompt = input.prompt ?? input.message ?? "";
    const trimmed = (prompt || "").trim();
    const isSystemMessage = trimmed.startsWith("<task-notification>")
      || trimmed.startsWith("<system-reminder>")
      || trimmed.startsWith("<context_guidance>")
      || trimmed.startsWith("<tool-result>");

    if (trimmed.length > 0 && !isSystemMessage) {
      // Front-load is deliberately independent from SessionDB. A DB/dependency
      // problem must not disable repository context, and a QC lookup failure
      // must never interfere with prompt capture.
      try {
        const { frontloadPromptContext } = await import("./qc-frontload.mjs");
        additionalContext = await frontloadPromptContext(trimmed, projectDir);
      } catch { /* fail open */ }

      try {
        const { SessionDB } = await loadSessionDB();
        const { extractUserEvents, extractUserPromptFeatures } = await loadExtract();
        const { resolveProjectAttributions } = await loadProjectAttribution();
        const db = new SessionDB({ dbPath: getSessionDBPath() });
        const sessionId = getSessionId(input);
        db.ensureSession(sessionId, projectDir);

        const promptFeatures = typeof extractUserPromptFeatures === "function"
          ? extractUserPromptFeatures(trimmed)
          : {};
        const promptEvent = {
          type: "user_prompt", category: "user-prompt", data: prompt, priority: 1, ...promptFeatures,
        };
        const promptAttributions = attributeAndInsertEvents(
          db, sessionId, [promptEvent], input, projectDir, "UserPromptSubmit", resolveProjectAttributions,
        );

        const userEvents = extractUserEvents(trimmed);
        const savedLastKnown = promptAttributions[0]?.projectDir || null;
        const sessionStats = db.getSessionStats(sessionId);
        const lastKnownProjectDir = typeof db.getLatestAttributedProjectDir === "function"
          ? db.getLatestAttributedProjectDir(sessionId)
          : null;
        resolveProjectAttributions(userEvents, {
          sessionOriginDir: sessionStats?.project_dir || projectDir,
          inputProjectDir: projectDir,
          workspaceRoots: Array.isArray(input.workspace_roots) ? input.workspace_roots : [],
          lastKnownProjectDir: savedLastKnown || lastKnownProjectDir,
        });
        if (userEvents.length > 0) {
          attributeAndInsertEvents(
            db, sessionId, userEvents, input, projectDir, "UserPromptSubmit", resolveProjectAttributions,
          );
        }
        db.close();
      } catch { /* session continuity is best-effort */ }
    }
  } catch { /* UserPromptSubmit must never block the session */ }

  if (additionalContext) {
    process.stdout.write(JSON.stringify({
      hookSpecificOutput: {
        hookEventName: "UserPromptSubmit",
        additionalContext,
      },
    }));
  }
});
