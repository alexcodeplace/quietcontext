#!/usr/bin/env bash
# Install + start the QuietContext shared HTTP daemon as a systemd --user unit.
# Symlinks the unit out of the repo (units stay source-controlled), reloads,
# enables, and verifies /healthz. Fail-closed: any missing piece aborts.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNIT_NAME="quietcontext-daemon.service"
UNIT_SRC="$REPO_ROOT/systemd/$UNIT_NAME"
UNIT_DIR="$HOME/.config/systemd/user"
PORT="${QUIET_CONTEXT_DAEMON_PORT:-48619}"

[[ -f "$UNIT_SRC" ]] || { echo "missing $UNIT_SRC" >&2; exit 1; }
[[ -f "$REPO_ROOT/http-server.bundle.mjs" ]] || {
  echo "missing http-server.bundle.mjs — run 'npm run build' in $REPO_ROOT first" >&2
  exit 1
}

mkdir -p "$UNIT_DIR"
ln -sfn "$UNIT_SRC" "$UNIT_DIR/$UNIT_NAME"
systemctl --user daemon-reload
systemctl --user enable --now "$UNIT_NAME"
systemctl --user restart "$UNIT_NAME"

for i in $(seq 1 25); do
  if curl -sf -m 2 "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then
    echo "quietcontext daemon healthy on 127.0.0.1:$PORT"
    exit 0
  fi
  sleep 0.2
done
echo "daemon failed health check on port $PORT:" >&2
systemctl --user status "$UNIT_NAME" --no-pager >&2 || true
exit 1
