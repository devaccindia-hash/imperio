@echo off
REM ============================================================
REM  Run this on the PC you are SITTING AT (it shows the other screen).
REM  Just double-click it. A small window asks for the host's address
REM  (the 100.x Tailscale one, or 192.168.x) and the code, if any.
REM ============================================================
title Imperio Viewer
cd /d "%~dp0"

echo.
echo   A connect window will open. Look at the HOST PC's window for
echo   its Address (and Code, unless it is in open mode).
echo.
echo   In the session: F11 = full screen, Ctrl+Shift+1..9 = switch display,
echo   Session menu or closing the window = disconnect.
echo.

"%~dp0imperio-viewer.exe" %*

echo.
echo Disconnected.
pause
