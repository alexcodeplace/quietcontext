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
$vbsPath = Join-Path $scriptDir "quietcontext-daemon.vbs"
$launcherPath = Join-Path $scriptDir "quietcontext-daemon.mjs"
$logPath = Join-Path $env:USERPROFILE ".local\state\quietcontext\daemon.log"
$userId = "$env:USERDOMAIN\$env:USERNAME"

function Get-DaemonTask {
    Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
}

function Get-ListeningProcessId {
    param([int]$OnPort)
    $connection = Get-NetTCPConnection -LocalPort $OnPort -State Listen -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($connection) { return $connection.OwningProcess }
    return $null
}

function Stop-DaemonProcess {
    param([int]$OnPort)
    # Task Scheduler does not reap the daemon through the wscript -> cmd -> node
    # chain, so stopping the task leaves the listener bound. Walk the chain and
    # stop every process that is demonstrably ours.
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
    if ($daemonPid) { Write-Host "daemon  : listening on 127.0.0.1:$Port (pid $daemonPid)" }
    else { Write-Host "daemon  : not listening on 127.0.0.1:$Port" }

    if (Test-Path -LiteralPath $logPath) {
        Write-Host "log     : $logPath"
        Get-Content -LiteralPath $logPath -Tail 5 | ForEach-Object { Write-Host "          $_" }
    }
    else {
        Write-Host "log     : $logPath (absent)"
    }

    if ($daemonPid) { exit 0 } else { exit 1 }
}

if ($Uninstall) {
    $task = Get-DaemonTask
    if (-not $task) {
        Write-Host "Task '$TaskName' is not registered — nothing to remove."
        exit 0
    }
    if ($task.State -eq "Running") { Stop-ScheduledTask -TaskName $TaskName }
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    if (Stop-DaemonProcess -OnPort $Port) { Write-Host "Stopped the daemon on 127.0.0.1:$Port." }
    Write-Host "Removed scheduled task '$TaskName'. Daemon state under ~/.local/state/quietcontext was left in place."
    exit 0
}

foreach ($required in @($vbsPath, $launcherPath)) {
    if (-not (Test-Path -LiteralPath $required)) { throw "missing file: $required" }
}

$nodeExe = Resolve-NodeExe

$action = New-ScheduledTaskAction `
    -Execute (Join-Path $env:SystemRoot "System32\wscript.exe") `
    -Argument ('"{0}" "{1}" "{2}"' -f $vbsPath, $nodeExe, $Port)

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

if (Get-DaemonTask) { Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false }

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $action `
    -Trigger $trigger `
    -Principal $principal `
    -Settings $settings `
    -Description "Shared QuietContext MCP daemon on 127.0.0.1:$Port" | Out-Null

Write-Host "Registered scheduled task '$TaskName' (node: $nodeExe, port: $Port)."

if ($NoStart) {
    Write-Host "Skipped start (-NoStart). It will run at next logon."
    exit 0
}

Start-ScheduledTask -TaskName $TaskName

$deadline = (Get-Date).AddSeconds(20)
do {
    Start-Sleep -Milliseconds 500
    $daemonPid = Get-ListeningProcessId -OnPort $Port
} while (-not $daemonPid -and (Get-Date) -lt $deadline)

if ($daemonPid) {
    Write-Host "Daemon listening on 127.0.0.1:$Port (pid $daemonPid)."
    exit 0
}

Write-Warning "Task started but nothing is listening on 127.0.0.1:$Port after 20s. Check $logPath"
exit 1
