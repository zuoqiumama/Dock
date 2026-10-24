@echo off
setlocal
pushd "%~dp0.." || exit /b 1
"%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoLogo -NoProfile -ExecutionPolicy Bypass -NoExit -File "%~dp0dev-env.ps1"
set "DOCK_SHELL_EXIT=%ERRORLEVEL%"
popd
exit /b %DOCK_SHELL_EXIT%
