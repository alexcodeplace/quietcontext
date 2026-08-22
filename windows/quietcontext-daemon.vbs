' Console-less shell for the QuietContext daemon launcher, invoked by Windows
' Task Scheduler through wscript.exe. Waits on the child so the task stays in
' the Running state and its restart-on-failure settings apply to the daemon
' itself rather than to a spawn that already returned.
'
' Usage: wscript.exe quietcontext-daemon.vbs [nodeExe] [port]
Option Explicit

Dim fso, sh, scriptDir, nodeExe, port, logDir, logFile, command

Set fso = CreateObject("Scripting.FileSystemObject")
Set sh = CreateObject("WScript.Shell")

scriptDir = fso.GetParentFolderName(WScript.ScriptFullName)

If WScript.Arguments.Count > 0 And Len(WScript.Arguments(0)) > 0 Then
  nodeExe = WScript.Arguments(0)
Else
  nodeExe = "node.exe"
End If

If WScript.Arguments.Count > 1 And Len(WScript.Arguments(1)) > 0 Then
  port = WScript.Arguments(1)
Else
  port = ""
End If

logDir = fso.BuildPath(fso.BuildPath(sh.ExpandEnvironmentStrings("%USERPROFILE%"), ".local"), "state")
logDir = fso.BuildPath(logDir, "quietcontext")
EnsureFolder logDir
logFile = fso.BuildPath(logDir, "daemon.log")

command = """" & nodeExe & """ """ & fso.BuildPath(scriptDir, "quietcontext-daemon.mjs") & _
          """ >> """ & logFile & """ 2>&1"
If Len(port) > 0 Then
  command = "set QUIET_CONTEXT_DAEMON_PORT=" & port & " && " & command
End If

WScript.Quit sh.Run("cmd /s /c """ & command & """", 0, True)

Sub EnsureFolder(path)
  Dim parent
  If fso.FolderExists(path) Then Exit Sub
  parent = fso.GetParentFolderName(path)
  If Len(parent) > 0 And Not fso.FolderExists(parent) Then EnsureFolder parent
  fso.CreateFolder path
End Sub
