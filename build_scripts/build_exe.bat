@echo off
setlocal

REM Change to project root directory (parent of build_scripts)
cd /d "%~dp0.."

echo ============================================
echo TurboMerger v4.0 Build (Onefile Mode)
echo ============================================
echo.

REM Check if uv is available
where uv >nul 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo ERROR: uv is not installed or not in PATH
    echo Please install uv: powershell -ExecutionPolicy ByPass -c "irm https://astral.sh/uv/install.ps1 | iex"
    pause
    exit /b 1
)

echo Syncing dependencies...
uv sync
if %ERRORLEVEL% NEQ 0 (
    echo ERROR: Failed to sync dependencies
    pause
    exit /b 1
)

echo.
echo Building with Nuitka (this may take several minutes)...
echo.

REM CRITICAL: Use "uv run python -m nuitka" NOT "uv run nuitka"
uv run python -m nuitka ^
    --standalone ^
    --onefile ^
    --enable-plugin=tk-inter ^
    --windows-console-mode=disable ^
    --company-name="TurboMerger" ^
    --product-name="TurboMerger" ^
    --file-version=4.0.0.0 ^
    --product-version=4.0.0.0 ^
    --file-description="Code merger for LLM context" ^
    --copyright="Copyright 2025 Pawel - MIT License" ^
    --output-dir=dist ^
    src\turbomerger\__main__.py

if %ERRORLEVEL% EQU 0 (
    REM Rename the executable
    if exist "dist\__main__.exe" (
        if exist "dist\turbomerger.exe" del "dist\turbomerger.exe"
        ren "dist\__main__.exe" "turbomerger.exe"
    )
    echo.
    echo ============================================
    echo SUCCESS: dist\turbomerger.exe
    echo ============================================
) else (
    echo.
    echo ============================================
    echo FAILED with error %ERRORLEVEL%
    echo ============================================
)

pause
