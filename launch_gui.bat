@echo off
cd /d "%~dp0"
if exist ".venv\Scripts\pythonw.exe" (
    start "" /B ".venv\Scripts\pythonw.exe" -m turbomerger
) else (
    start "" pythonw -m turbomerger
)
exit
