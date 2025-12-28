@echo off
setlocal

REM Change to project root directory (parent of build_scripts)
cd /d "%~dp0.."

echo ============================================
echo TurboMerger v4.0 Build (Standalone Folder Mode)
echo ============================================
echo.
echo This mode creates a folder instead of a single exe.
echo Fewer antivirus false positives but larger output.
echo.

REM Check if uv is available
where uv >nul 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo ERROR: uv is not installed or not in PATH
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
echo Building with Nuitka (standalone folder mode)...
echo.

uv run python -m nuitka ^
    --standalone ^
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
    REM Rename the folder and exe
    if exist "dist\__main__.dist" (
        if exist "dist\turbomerger" rmdir /s /q "dist\turbomerger"
        ren "dist\__main__.dist" "turbomerger"
        if exist "dist\turbomerger\__main__.exe" (
            ren "dist\turbomerger\__main__.exe" "turbomerger.exe"
        )
    )
    echo.
    echo ============================================
    echo SUCCESS: dist\turbomerger\turbomerger.exe
    echo ============================================
) else (
    echo.
    echo FAILED with error %ERRORLEVEL%
)

pause
