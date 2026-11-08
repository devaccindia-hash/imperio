@echo off
REM ============================================================
REM  Run this on the PC you want to CONTROL (it shares its screen).
REM  Just double-click it. It will ask for admin (for the firewall).
REM ============================================================
title Imperio Host

REM --- Re-launch as administrator if we aren't already ---
net session >nul 2>&1
if %errorlevel% neq 0 (
    echo Asking for administrator access ^(needed to open the firewall^)...
    powershell -NoProfile -Command "Start-Process -FilePath '%~f0' -Verb RunAs"
    exit /b
)

cd /d "%~dp0"

REM --- Allow this program through the Windows firewall on UDP 7700 ---
netsh advfirewall firewall delete rule name="Imperio Host" >nul 2>&1
netsh advfirewall firewall add rule name="Imperio Host" dir=in action=allow protocol=UDP localport=7700 >nul

echo.
echo ===============================================================
echo   On the OTHER PC, run 2-START-VIEWER.bat and type the
echo   Address and Code shown just below.
echo   Keep this window open. Press Ctrl+C here to stop sharing.
echo ===============================================================

"%~dp0imperio-host.exe" --open

echo.
echo Imperio host stopped.
pause
