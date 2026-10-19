@echo off
powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command "try { $ErrorActionPreference='Stop'; $barTask=Get-ScheduledTask -TaskName 'Codex Session Bar'; if ($barTask.State -eq 'Running') { $barAction=$barTask.Actions[0]; $barStart=@{FilePath=$barAction.Execute;WorkingDirectory=$barAction.WorkingDirectory;WindowStyle='Hidden'}; if ($barAction.Arguments) { $barStart.ArgumentList=$barAction.Arguments }; Start-Process @barStart } else { Start-ScheduledTask -TaskName 'Codex Session Bar' }; exit 0 } catch { exit 1 }"
if not errorlevel 1 exit /b
if exist "%~dp0bin\codex-session-bar.exe" (
  start "" "%~dp0bin\codex-session-bar.exe"
  exit /b
)
echo The Rust executable was not found. Run Build.ps1 first.
pause
