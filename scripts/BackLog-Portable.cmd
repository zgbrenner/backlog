@echo off
setlocal

set "BACKLOG_ROOT=%~dp0"
set "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER=%BACKLOG_ROOT%webview2-fixed"

if "%BACKLOG_ROOT:~0,2%"=="\\" (
  echo Fixed WebView2 cannot run from a network or UNC path.
  echo Extract the ZIP to a local NTFS folder before launching BackLog.
  exit /b 4
)

if not exist "%BACKLOG_ROOT%BackLog.exe" (
  echo BackLog.exe is missing from this portable directory.
  exit /b 2
)
if not exist "%WEBVIEW2_BROWSER_EXECUTABLE_FOLDER%\msedgewebview2.exe" (
  echo The bundled fixed WebView2 runtime is missing.
  echo Re-extract the complete BackLog portable ZIP before launching.
  exit /b 3
)

rem Fixed WebView2 120+ on Windows 10 needs AppContainer read/execute access.
for /f "tokens=3" %%B in ('reg query "HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion" /v CurrentBuildNumber 2^>nul ^| findstr /C:"CurrentBuildNumber"') do set "BACKLOG_BUILD=%%B"
if defined BACKLOG_BUILD call :GrantFixedRuntimeAccess

pushd "%BACKLOG_ROOT%" >nul
"%BACKLOG_ROOT%BackLog.exe" %*
set "BACKLOG_EXIT_CODE=%ERRORLEVEL%"
popd >nul
exit /b %BACKLOG_EXIT_CODE%

:GrantFixedRuntimeAccess
if %BACKLOG_BUILD% GEQ 22000 exit /b 0
icacls "%WEBVIEW2_BROWSER_EXECUTABLE_FOLDER%" /grant "*S-1-15-2-2:(OI)(CI)(RX)" /T >nul 2>&1
if errorlevel 1 (
  echo Could not grant WebView2 access to ALL APPLICATION PACKAGES.
  echo Extract the ZIP to a local NTFS folder owned by this user.
  exit /b 5
)
icacls "%WEBVIEW2_BROWSER_EXECUTABLE_FOLDER%" /grant "*S-1-15-2-1:(OI)(CI)(RX)" /T >nul 2>&1
if errorlevel 1 (
  echo Could not grant WebView2 access to ALL RESTRICTED APPLICATION PACKAGES.
  echo Extract the ZIP to a local NTFS folder owned by this user.
  exit /b 6
)
exit /b 0
