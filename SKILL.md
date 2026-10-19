---
name: codex-session-bar
description: Open or restore the installed Windows Codex Session Bar when the user says 打开会话条, 打开悬浮条, 恢复工具条, or open session bar. Handles a hidden bar or a fully exited process. Do not use for general session-bar development.
---

# Open Codex Session Bar

Use the installed current-user scheduled task and its local Windows show event. The user does not need to locate or execute a script. Do not restart Codex, interrupt chats, change proxy settings, or reconfigure the task.

Run the following PowerShell. It starts an exited bar or signals an existing one, then checks its fresh diagnostic state. Temporary writes are unnecessary.

```powershell
$ErrorActionPreference = 'Stop'
$barTask = Get-ScheduledTask -TaskName 'Codex Session Bar'
if ($barTask.State -ne 'Running') { Start-ScheduledTask -TaskName 'Codex Session Bar' }
$barHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
$barHome = (Resolve-Path -LiteralPath $barHome).Path.ToLowerInvariant()
$barHasher = [Security.Cryptography.SHA256]::Create()
try { $barHash = ([BitConverter]::ToString($barHasher.ComputeHash([Text.Encoding]::UTF8.GetBytes($barHome)))).Replace('-','').ToLowerInvariant().Substring(0,12) } finally { $barHasher.Dispose() }
$barEventName = 'Local\codex-session-bar-' + $barHash + '-show'
$barDeadline = [DateTime]::UtcNow.AddSeconds(12)
$barSignal = $null
while (-not $barSignal -and [DateTime]::UtcNow -lt $barDeadline) {
    try { $barSignal = [Threading.EventWaitHandle]::OpenExisting($barEventName) }
    catch [Threading.WaitHandleCannotBeOpenedException] { Start-Sleep -Milliseconds 200 }
}
if (-not $barSignal) { throw 'Session Bar did not expose its show event. Inspect its runtime error before retrying.' }
$barRequestedAt = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()/1000.0
try { [void]$barSignal.Set() } finally { $barSignal.Dispose() }
$barDirectory = $barTask.Actions[0].WorkingDirectory
if (-not $barDirectory) { throw 'Task working directory is unavailable; inspect the installed task.' }
$barWorkspace = if ($env:CODEX_SESSION_BAR_WORKSPACE) { $env:CODEX_SESSION_BAR_WORKSPACE } elseif ((Split-Path -Leaf (Split-Path -Parent $barDirectory)) -eq 'tools') { Split-Path -Parent (Split-Path -Parent $barDirectory) } else { $barDirectory }
$barDiagnostic = Join-Path $barWorkspace 'tmp/codex-session-bar/live-status.json'
$barStatus = $null
while ([DateTime]::UtcNow -lt $barDeadline) {
    if (Test-Path -LiteralPath $barDiagnostic) {
        try { $barStatus = Get-Content -LiteralPath $barDiagnostic -Raw | ConvertFrom-Json } catch { $barStatus = $null }
        if ($barStatus -and $barStatus.updated_at -ge $barRequestedAt -and $barStatus.user_hidden -eq $false) { break }
    }
    Start-Sleep -Milliseconds 200
}
if (-not $barStatus -or $barStatus.updated_at -lt $barRequestedAt -or $barStatus.user_hidden -ne $false) { throw 'Show request was sent, but restoration was not confirmed.' }
[pscustomobject]@{Restored=$true;HostVisible=(-not $barStatus.host_hidden);PID=$barStatus.pid}
```

Confirm it is open only after `Restored=True`. If `HostVisible=False`, explain that the bar is ready and will appear when the Codex main window is restored. If the scheduled task is missing, report that installation is required; do not guess another installation path or silently create a replacement. Stop after one failed attempt and inspect the specific error rather than retrying repeatedly.
