@echo off
REM TurboMerger v4.0 - One-Click Dependency Installation
REM Installs uv (if needed) and project dependencies

echo ============================================
echo   TurboMerger v4.0 - Dependency Installer
echo ============================================
echo.

REM Check if uv is installed
where uv >nul 2>&1
if %errorlevel% neq 0 (
    echo [*] uv not found, installing...
    powershell -ExecutionPolicy Bypass -Command "irm https://astral.sh/uv/install.ps1 | iex"
    if %errorlevel% neq 0 (
        echo [ERROR] Failed to install uv
        echo Please install manually: https://docs.astral.sh/uv/getting-started/installation/
        pause
        exit /b 1
    )
    echo [OK] uv installed successfully
    echo.
    echo NOTE: You may need to restart this terminal for uv to be available.
    echo.
) else (
    echo [OK] uv is already installed
)

echo.
echo [*] Installing project dependencies...
uv sync
if %errorlevel% neq 0 (
    echo [ERROR] Failed to install dependencies
    pause
    exit /b 1
)

echo.
echo ============================================
echo   Installation Complete!
echo ============================================
echo.
echo To run TurboMerger:
echo   - GUI:  double-click launch_gui.vbs (no console)
echo   - GUI:  launch_gui.bat (with console for debugging)
echo   - CLI:  uv run python -m turbomerger --help
echo.
pause
