@echo off
setlocal DisableDelayedExpansion
set "terminalnote_selfremove="
if exist "%~dp0.terminalnote-install.json" set "terminalnote_selfremove=1"
if defined terminalnote_selfremove (
    "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" -KeepLauncher %*
) else (
    "%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0uninstall.ps1" %*
)
set "uninstall_exit=%errorlevel%"
if "%~1"=="" pause
if not "%uninstall_exit%"=="0" exit /b %uninstall_exit%
if not defined terminalnote_selfremove exit /b 0
if exist "%~dp0.terminalnote-install.json" exit /b 0
rem Delete only this launcher, then the directory ONLY if empty. Never use rd /s.
(goto) 2>nul & del "%~f0" & rd "%~dp0" 2>nul & exit /b 0
