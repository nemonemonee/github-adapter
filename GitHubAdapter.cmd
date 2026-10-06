@echo off
setlocal DisableDelayedExpansion
cd /d "%~dp0"
if not defined LOCALAPPDATA (
  echo GitHub Adapter cannot locate the current user's native installation.
  exit /b 1
)
set "adapter_native=%LOCALAPPDATA%\GitHubAdapter\bin\github-adapter.exe"
if not exist "%adapter_native%" (
  echo GitHub Adapter's native executable is not installed.
  echo Run tools\install-native.ps1 after building the native release.
  exit /b 1
)
"%adapter_native%" %*
set "exit_code=%errorlevel%"
if not "%exit_code%"=="0" (
  echo GitHub Adapter exited with error code %exit_code%.
)
exit /b %exit_code%
