<#
.SYNOPSIS
Registers the shared QuietContext MCP daemon as a per-user logon task.

.DESCRIPTION
Windows equivalent of systemd/quietcontext-daemon.service. The daemon keeps
per-user state under the user profile, so it runs as the logged-on user rather
than as a machine service under SYSTEM.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\Install-QuietContextDaemon.ps1

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\Install-QuietContextDaemon.ps1 -Status

.EXAMPLE
powershell -ExecutionPolicy Bypass -File .\Install-QuietContextDaemon.ps1 -Uninstall
#>
[CmdletBinding(DefaultParameterSetName = "Install")]
param(
    [ValidateRange(1, 65535)]
    [int]$Port = 48619,

    [Parameter(ParameterSetName = "Install")]
    [string]$NodePath,

    [Parameter(ParameterSetName = "Install")]
    [switch]$NoStart,

    [Parameter(ParameterSetName = "Uninstall", Mandatory = $true)]
    [switch]$Uninstall,

    [Parameter(ParameterSetName = "Status", Mandatory = $true)]
    [switch]$Status,

    [string]$TaskName = "QuietContext Daemon"
)

$ErrorActionPreference = "Stop"

$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$sourceVbsPath = Join-Path $scriptDir "quietcontext-daemon.vbs"
$sourceLauncherPath = Join-Path $scriptDir "quietcontext-daemon.mjs"
$runtimeDir = Join-Path $env:LOCALAPPDATA "QuietContext"
$runtimeVbsPath = Join-Path $runtimeDir "quietcontext-daemon.vbs"
$runtimeLauncherPath = Join-Path $runtimeDir "quietcontext-daemon.mjs"
$logPath = Join-Path $env:USERPROFILE ".local\state\quietcontext\daemon.log"
$userId = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name

function Get-DaemonTask {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}

function Get-ListeningProcessId {
    param([int]$OnPort)
    $connection = Get-NetTCPConnection -LocalAddress "127.0.0.1" -LocalPort $OnPort -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($connection) { return $connection.OwningProcess }
    return $null
}

function Test-QuietContextHealth {
    param([int]$OnPort)
    try {
        $health = Invoke-RestMethod -Uri "http://127.0.0.1:$OnPort/healthz" -Method Get -TimeoutSec 2 -ErrorAction Stop
        return ($health.ok -eq $true -and $health.name -eq "quietcontext")
    }
    catch {
        return $false
    }
}

function Stop-DaemonProcess {
    param([int]$OnPort)
    # Task Scheduler does not reliably reap the daemon through the
    # wscript -> cmd -> node chain. Walk the chain and stop only processes
    # whose command line demonstrates that they belong to this launcher.
    $daemonPid = Get-ListeningProcessId -OnPort $OnPort
    if (-not $daemonPid) { return $false }

    $processes = @()
    $current = Get-CimInstance Win32_Process -Filter "ProcessId = $daemonPid" -ErrorAction SilentlyContinue
    while ($current) {
        if ($current.CommandLine -notlike "*quietcontext-daemon*") { break }
        $processes += $current
        $current = Get-CimInstance Win32_Process -Filter "ProcessId = $($current.ParentProcessId)" -ErrorAction SilentlyContinue
    }

    if (-not $processes) {
        Write-Warning "127.0.0.1:$OnPort is held by pid $daemonPid, which is not a QuietContext daemon — left running."
        return $false
    }

    foreach ($process in $processes) {
        Stop-Process -Id $process.ProcessId -Force -ErrorAction SilentlyContinue
    }
    return $true
}

function Resolve-NodeExe {
    if ($NodePath) {
        if (-not (Test-Path -LiteralPath $NodePath)) { throw "node executable not found: $NodePath" }
        return (Resolve-Path -LiteralPath $NodePath).Path
    }
    $command = Get-Command node.exe -ErrorAction SilentlyContinue
    if (-not $command) { throw "node.exe not found on PATH — install Node.js or pass -NodePath" }
    return $command.Source
}

if ($Status) {
    $task = Get-DaemonTask
    if (-not $task) {
        Write-Host "task    : not registered ($TaskName)"
    }
    else {
        $info = Get-ScheduledTaskInfo -TaskName $TaskName
        $lastResult = "0x{0:X8}" -f $info.LastTaskResult
        Write-Host "task    : $($task.State) (last result $lastResult, last run $($info.LastRunTime))"
    }

    $daemonPid = Get-ListeningProcessId -OnPort $Port
    $healthy = Test-QuietContextHealth -OnPort $Port
    if ($healthy) {
        Write-Host "daemon  : QuietContext healthy on 127.0.0.1:$Port (pid $daemonPid)"
    }
    elseif ($daemonPid) {
        Write-Host "daemon  : unhealthy/foreign listener on 127.0.0.1:$Port (pid $daemonPid)"
    }
    else {
        Write-Host "daemon  : not listening on 127.0.0.1:$Port"
    }

    if (Test-Path -LiteralPath $logPath) {
        Write-Host "log     : $logPath"
        Get-Content -LiteralPath $logPath -Tail 5 | ForEach-Object { Write-Host "          $_" }
    }
    else {
        Write-Host "log     : $logPath (absent)"
    }

    if ($healthy) { exit 0 } else { exit 1 }
}

if ($Uninstall) {
    $task = Get-DaemonTask
    if ($task) {
        if ($task.State -eq "Running") {
            Stop-ScheduledTask -TaskName $TaskName
            Start-Sleep -Milliseconds 250
        }
        Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
        Write-Host "Removed scheduled task '$TaskName'."
    }
    else {
        Write-Host "Task '$TaskName' is not registered."
    }

    if (Stop-DaemonProcess -OnPort $Port) { Write-Host "Stopped the daemon on 127.0.0.1:$Port." }
    foreach ($runtimePath in @($runtimeVbsPath, $runtimeLauncherPath)) {
        Remove-Item -LiteralPath $runtimePath -Force -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $runtimeDir) {
        $remaining = Get-ChildItem -LiteralPath $runtimeDir -Force -ErrorAction SilentlyContinue
        if (-not $remaining) { Remove-Item -LiteralPath $runtimeDir -Force -ErrorAction SilentlyContinue }
    }
    Write-Host "Daemon state under ~/.local/state/quietcontext was left in place."
    exit 0
}

foreach ($required in @($sourceVbsPath, $sourceLauncherPath)) {
    if (-not (Test-Path -LiteralPath $required)) { throw "missing file: $required" }
}

$nodeExe = Resolve-NodeExe

# Reinstall cleanly: stop an existing scheduled instance and its descendant
# daemon before replacing the task. Otherwise the replacement task can exit
# immediately against the old listener and leave that daemon unsupervised.
$existingTask = Get-DaemonTask
if ($existingTask) {
    if ($existingTask.State -eq "Running") {
        Stop-ScheduledTask -TaskName $TaskName
        Start-Sleep -Milliseconds 250
    }
    Stop-DaemonProcess -OnPort $Port | Out-Null
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
}

# The plugin cache is versioned and old versions may be deleted during an
# upgrade. Keep the tiny scheduler bootstrap in a stable per-user location;
# the JS launcher itself resolves the current plugin version on every start.
New-Item -ItemType Directory -Path $runtimeDir -Force | Out-Null
Copy-Item -LiteralPath $sourceVbsPath -Destination $runtimeVbsPath -Force
Copy-Item -LiteralPath $sourceLauncherPath -Destination $runtimeLauncherPath -Force

$action = New-ScheduledTaskAction `
    -Execute (Join-Path $env:SystemRoot "System32\wscript.exe") `
    -Argument ('"{0}" "{1}" "{2}"' -f $runtimeVbsPath, $nodeExe, $Port)

$trigger = New-ScheduledTaskTrigger -AtLogOn -User $userId

$principal = New-ScheduledTaskPrincipal -UserId $userId -LogonType Interactive -RunLevel Limited

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -RestartCount 3 `
    -RestartInterval (New-TimeSpan -Minutes 1) `
    -ExecutionTimeLimit ([TimeSpan]::Zero) `
    -MultipleInstances IgnoreNew

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $action `
    -Trigger $trigger `
    -Principal $principal `
    -Settings $settings `
    -Description "Shared QuietContext MCP daemon on 127.0.0.1:$Port" | Out-Null

Write-Host "Registered scheduled task '$TaskName' (node: $nodeExe, port: $Port)."
Write-Host "Runtime bootstrap: $runtimeDir"

if ($NoStart) {
    Write-Host "Skipped start (-NoStart). It will run at next logon."
    exit 0
}

Start-ScheduledTask -TaskName $TaskName

$deadline = (Get-Date).AddSeconds(20)
do {
    Start-Sleep -Milliseconds 500
    $healthy = Test-QuietContextHealth -OnPort $Port
} while (-not $healthy -and (Get-Date) -lt $deadline)

$daemonPid = Get-ListeningProcessId -OnPort $Port
if ($healthy) {
    Write-Host "QuietContext healthy on 127.0.0.1:$Port (pid $daemonPid)."
    exit 0
}

if ($daemonPid) {
    Write-Warning "Task started but 127.0.0.1:$Port is not a healthy QuietContext daemon (pid $daemonPid). Check $logPath"
}
else {
    Write-Warning "Task started but nothing is listening on 127.0.0.1:$Port after the startup wait. Check $logPath"
}
exit 1
