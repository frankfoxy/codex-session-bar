@echo off
powershell.exe -NoProfile -NonInteractive -WindowStyle Hidden -Command "try { $ErrorActionPreference='Stop'; Start-ScheduledTask -TaskName 'Codex Session Bar'; exit 0 } catch { exit 1 }"
if not errorlevel 1 exit /b
if defined CODEX_SESSION_BAR_PYTHON (
  start "" "%CODEX_SESSION_BAR_PYTHON%" -B "%~dp0session_bar.py"
  exit /b
)
where pythonw.exe >nul 2>nul
if not errorlevel 1 (
  start "" pythonw.exe -B "%~dp0session_bar.py"
  exit /b
)
where pyw.exe >nul 2>nul
if not errorlevel 1 (
  start "" pyw.exe -3 -B "%~dp0session_bar.py"
  exit /b
)
echo Python was not found. Install Python 3.10+ and the packages in requirements.txt.
pause
