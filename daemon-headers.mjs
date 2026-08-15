#!/usr/bin/env node
// headersHelper for the shared QuietContext HTTP daemon: prints the bearer
// token as a headers JSON object. Mirrors ensureDaemonToken (src/http-server.ts)
// — read the 0600 token file, create it atomically (O_EXCL) when absent, so
// whichever side runs first wins the race safely. Dependency-free: ships raw
// in the plugin and runs outside the bundle.
import { closeSync, mkdirSync, openSync, readFileSync, writeSync } from "node:fs";
import { randomBytes } from "node:crypto";
import { dirname, join } from "node:path";
import { homedir } from "node:os";

const tokenFile =
  process.env.QUIET_CONTEXT_DAEMON_TOKEN_FILE ||
  join(homedir(), ".local", "state", "quietcontext", "daemon.token");

const read = () => {
  try {
    const raw = readFileSync(tokenFile, "utf8").trim();
    return raw.length >= 32 ? raw : null;
  } catch {
    return null;
  }
};

let token = read();
if (!token) {
  mkdirSync(dirname(tokenFile), { recursive: true, mode: 0o700 });
  const fresh = randomBytes(32).toString("hex");
  try {
    const fd = openSync(tokenFile, "wx", 0o600);
    try {
      writeSync(fd, fresh + "\n");
    } finally {
      closeSync(fd);
    }
    token = fresh;
  } catch {
    token = read();
  }
}
if (!token) {
  console.error(`quietcontext daemon-headers: cannot read or create ${tokenFile}`);
  process.exit(1);
}
console.log(JSON.stringify({ Authorization: `Bearer ${token}` }));
