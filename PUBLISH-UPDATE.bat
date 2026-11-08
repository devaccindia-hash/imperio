@echo off
REM ============================================================
REM  Publish an Imperio update  (no code needed)
REM  Double-click. It asks for the new version and what changed,
REM  then builds, signs, and deploys to https://getimperio.vercel.app
REM ============================================================
title Publish Imperio update
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0tools\publish-update.ps1"
echo.
pause
