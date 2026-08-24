# Windows daemon autostart

Windows counterpart of `systemd/quietcontext-daemon.service`. Without it the
plugin manifest points at `http://127.0.0.1:48619/mcp` and nothing ever binds
that port on Windows, so every tool call fails.

## Install

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\Install-QuietContextDaemon.ps1
```

Registers a per-user logon task named `QuietContext Daemon`, starts it, and
waits until the daemon is listening. Flags: `-Port`, `-NodePath`, `-TaskName`,
`-NoStart`, `-Status`, `-Uninstall`.

`-Status` prints the task state, the listening pid, and the tail of the log,
exiting non-zero when the daemon is down.

## Files

- `Install-QuietContextDaemon.ps1` — registers, inspects, and removes the task.
- `quietcontext-daemon.vbs` — runs the launcher through `cmd` with no console
  window and waits on it, so the task stays `Running` and Task Scheduler's
  restart-on-failure settings apply to the daemon rather than to a spawn that
  already returned. Appends stdout and stderr to
  `%USERPROFILE%\.local\state\quietcontext\daemon.log`, next to the daemon
  token file.
- `quietcontext-daemon.mjs` — resolves the installed plugin's current version
  directory from `installed_plugins.json` (falling back to a cache scan) and
  imports its `start-http.mjs`. If the port is already bound, it verifies the
  `/healthz` identity and fails rather than treating an unrelated listener as
  QuietContext.

## Task settings

Mapped from the systemd unit: `RestartCount 3` / `RestartInterval 1 minute`
for `Restart=on-failure` / `RestartSec=2`, `ExecutionTimeLimit 0` so a
long-lived daemon is never killed, and `MultipleInstances IgnoreNew` so a
manual start does not race the logon trigger.

## Why a logon task rather than a Windows service

The daemon keeps per-user state under the user profile — the FTS5 stores under
`~/.claude/context-mode/` and the `0600` token file under
`~/.local/state/quietcontext/`. A machine service running as `SYSTEM` resolves
a different home directory and would serve the wrong stores; running one as the
user requires storing that user's password. A logon task needs neither.

## Version churn

The installer copies the small VBS/JS scheduler bootstrap to
`%LOCALAPPDATA%\QuietContext` and registers the task against that stable path.
The JS launcher re-resolves the current plugin directory on every start, so
removing an old versioned plugin cache does not strand the task.

## Uninstall

```powershell
powershell -ExecutionPolicy Bypass -File .\windows\Install-QuietContextDaemon.ps1 -Uninstall
```

Removes the task and the stable bootstrap under `%LOCALAPPDATA%\QuietContext`.
State under `~/.local/state/quietcontext` and `~/.claude/context-mode` is left
in place.
