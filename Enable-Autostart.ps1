$ErrorActionPreference = 'Stop'
if ($env:CODEX_SESSION_BAR_WORKSPACE) {
    $workspace = (Resolve-Path -LiteralPath $env:CODEX_SESSION_BAR_WORKSPACE).Path
} elseif ((Split-Path -Leaf (Split-Path -Parent $PSScriptRoot)) -eq 'tools') {
    $workspace = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
} else {
    $workspace = (Resolve-Path -LiteralPath $PSScriptRoot).Path
}
$taskTemp = [IO.Path]::GetFullPath((Join-Path $workspace 'tmp/codex-session-bar'))
$tmpRoot = [IO.Path]::GetFullPath((Join-Path $workspace 'tmp'))
if (-not $taskTemp.StartsWith($tmpRoot + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
    throw 'Runtime must be inside workspace/tmp'
}
[IO.Directory]::CreateDirectory($taskTemp) | Out-Null
$env:TEMP = $taskTemp
$env:TMP = $taskTemp
$env:TMPDIR = $taskTemp
$pythonArgs = '-B '
if ($env:CODEX_SESSION_BAR_PYTHON) {
    $python = (Get-Command $env:CODEX_SESSION_BAR_PYTHON -ErrorAction Stop).Source
} elseif ($command = Get-Command pythonw.exe -ErrorAction SilentlyContinue) {
    $python = $command.Source
} elseif ($command = Get-Command pyw.exe -ErrorAction SilentlyContinue) {
    $python = $command.Source
    $pythonArgs = '-3 -B '
} else {
    throw 'Python was not found. Install Python 3.10+ and requirements.txt first.'
}
$scriptFile = Join-Path $PSScriptRoot 'session_bar.py'
$userId = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $python -Argument ($pythonArgs + '"' + $scriptFile + '" --capture-after 5') -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $userId
$principal = New-ScheduledTaskPrincipal -UserId $userId -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'Codex Session Bar' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description 'Compact Codex session bar. Waits while Codex is closed and follows its window when reopened.' -Force | Out-Null
Start-ScheduledTask -TaskName 'Codex Session Bar'
Get-ScheduledTask -TaskName 'Codex Session Bar' | Select-Object TaskName, State
