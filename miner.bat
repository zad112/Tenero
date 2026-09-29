@echo off
cd /d "%~dp0"

if exist ".venv\Scripts\python.exe" (
    ".venv\Scripts\python.exe" miner.py %*
) else (
    python miner.py %*
)

echo.
pause