@echo off
REM Run this on the HOST PC (the one you want to control) as Administrator.
REM It lets other PCs on your network reach Imperio on UDP 7700.
REM Right-click -> "Run as administrator".

netsh advfirewall firewall delete rule name="Imperio Host" >nul 2>&1
netsh advfirewall firewall add rule name="Imperio Host" dir=in action=allow protocol=UDP localport=7700
echo.
echo Done. UDP 7700 inbound is now allowed for Imperio.
echo To remove later: netsh advfirewall firewall delete rule name="Imperio Host"
echo.
pause
