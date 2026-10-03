@echo off
setlocal EnableExtensions
title QQ Music SMTC seek plugin
chcp 65001 >nul

REM ---------------------------------------------------------------------------
REM  One-click installer / uninstaller wrapper.
REM
REM  NOTE: keep this file ASCII-only. cmd.exe tracks its position in a .bat by
REM  byte offset, and with chcp 65001 a multi-byte character desyncs it, which
REM  makes cmd re-execute a fragment of the next line as a command. The Chinese
REM  messages live in the .ps1 files instead (they are read as UTF-8 with BOM).
REM ---------------------------------------------------------------------------

set "BAT=%~f0"
set "ROOT=%~dp0"
set "PS=powershell -NoProfile -ExecutionPolicy Bypass"
set "DLL=%ROOT%payload.dll"
if not exist "%DLL%" set "DLL="

set "ACT=%~1"
if not "%ACT%"=="" goto :dispatch

:menu
echo.
echo   QQ Music SMTC seek plugin
echo   folder: %ROOT%
echo.
echo     [1] install
echo     [2] uninstall
echo     [3] status check  (read only)
echo     [0] quit
echo.
set "ACT="
set /p ACT=   choose and press Enter: 
if "%ACT%"=="1" set "ACT=install"   & goto :elevate
if "%ACT%"=="2" set "ACT=uninstall" & goto :elevate
if "%ACT%"=="3" set "ACT=check"     & goto :do_check
if "%ACT%"=="0" exit /b 0
echo   unknown choice: %ACT%
goto :menu

:dispatch
if /i "%ACT%"=="install"   goto :elevate
if /i "%ACT%"=="uninstall" goto :elevate
if /i "%ACT%"=="check"     goto :do_check
echo   usage: "%~nx0" [install ^| uninstall ^| check]
exit /b 2

REM install/uninstall write into Program Files, which needs elevation.
:elevate
net session >nul 2>&1
if errorlevel 1 (
    echo.
    echo   administrator rights required, requesting elevation...
    %PS% "Start-Process -FilePath '%BAT%' -ArgumentList '%ACT%' -Verb RunAs"
    if errorlevel 1 echo   elevation declined, nothing was changed.
    exit /b
)

REM an elevated process starts in system32; go back to this script's folder.
cd /d "%ROOT%" || exit /b 1
goto :run

:run
if /i "%ACT%"=="install"   goto :do_install
if /i "%ACT%"=="uninstall" goto :do_uninstall
goto :do_check

:do_install
echo.
echo ===== install =====
if "%DLL%"=="" (
    echo   no payload.dll next to this script, using the build output instead.
    %PS% "& '%ROOT%scripts\install.ps1' -Restart"
) else (
    echo   using payload.dll next to this script.
    %PS% "& '%ROOT%scripts\install.ps1' -DllPath '%DLL%' -Restart"
)
echo.
echo   wait ~15 s, then run: "%~nx0" check
exit /b %errorlevel%

:do_uninstall
echo.
echo ===== uninstall =====
%PS% "& '%ROOT%scripts\uninstall.ps1'"
echo.
echo   log folder %TEMP%\QQMusicInjectorLogs was left in place.
exit /b %errorlevel%

:do_check
echo.
echo ===== status =====
%PS% "& '%ROOT%scripts\status.ps1'"
echo.
%PS% "& '%ROOT%probe\smtc-probe.ps1' -Match QQMusic" 2>nul | findstr /i /c:"SourceApp" /c:"Title" /c:"minSeek" /c:"pos="
exit /b 0
