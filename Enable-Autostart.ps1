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
$executable = Join-Path $PSScriptRoot 'bin/codex-session-bar.exe'
if (-not (Test-Path -LiteralPath $executable)) {
    throw 'Rust executable was not found. Build it with Build.ps1 first.'
}
$userId = [Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute $executable -WorkingDirectory $PSScriptRoot
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $userId
$principal = New-ScheduledTaskPrincipal -UserId $userId -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName 'Codex Session Bar' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description 'Compact Codex session bar. Waits while Codex is closed and follows its window when reopened.' -Force | Out-Null
$skillSource = Join-Path $PSScriptRoot 'SKILL.md'
if (Test-Path -LiteralPath $skillSource) {
    $skillHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
    $skillDirectory = Join-Path $skillHome 'skills/codex-session-bar'
    [IO.Directory]::CreateDirectory($skillDirectory) | Out-Null
    Copy-Item -LiteralPath $skillSource -Destination (Join-Path $skillDirectory 'SKILL.md') -Force
}
Start-ScheduledTask -TaskName 'Codex Session Bar'
Get-ScheduledTask -TaskName 'Codex Session Bar' | Select-Object TaskName, State
