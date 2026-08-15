#!/usr/bin/env bash
# Install + start the QuietContext shared HTTP daemon as a systemd --user unit.
# The daemon runs from the plugin deploy clone (~/.claude/plugins/sources/quietmode
# — the same tree the marketplace install republishes from), so daemon version and
# plugin version move together. The unit is symlinked out of that clone (units
# stay source-controlled), then enabled and health-checked. Fail-closed.
set -euo pipefail

DEPLOY_ROOT="${QUIET_CONTEXT_DEPLOY_ROOT:-$HOME/.claude/plugins/sources/quietmode}"
UNIT_NAME="quietcontext-daemon.service"
UNIT_SRC="$DEPLOY_ROOT/systemd/$UNIT_NAME"
UNIT_DIR="$HOME/.config/systemd/user"
PORT="${QUIET_CONTEXT_DAEMON_PORT:-48619}"

[[ -f "$UNIT_SRC" ]] || { echo "missing $UNIT_SRC — deploy clone not up to date" >&2; exit 1; }
[[ -f "$DEPLOY_ROOT/http-server.bundle.mjs" ]] || {
  echo "missing $DEPLOY_ROOT/http-server.bundle.mjs — run 'npm run build' in the deploy clone first" >&2
  exit 1
}
[[ -d "$DEPLOY_ROOT/node_modules/better-sqlite3" ]] || {
  echo "missing node_modules in $DEPLOY_ROOT — run 'npm install' in the deploy clone first" >&2
  exit 1
}

mkdir -p "$UNIT_DIR"
ln -sfn "$UNIT_SRC" "$UNIT_DIR/$UNIT_NAME"
systemctl --user daemon-reload
systemctl --user enable "$UNIT_NAME"
systemctl --user restart "$UNIT_NAME"

for _ in $(seq 1 25); do
  if curl -sf -m 2 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "quietcontext daemon healthy on 127.0.0.1:$PORT"
    exit 0
  fi
  sleep 0.2
done
echo "daemon failed health check on port $PORT:" >&2
systemctl --user status "$UNIT_NAME" --no-pager >&2 || true
exit 1
