# scripts/build_msi.ps1
# Builds the TurboMerger MSI installer

$ErrorActionPreference = "Stop"

$PROJECT_ROOT = "$PSScriptRoot\.."
$DIST_DIR = "$PROJECT_ROOT\src-tauri\target\release\bundle\msi"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  TurboMerger v5 Build Script" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

# Check prerequisites
Write-Host "Checking prerequisites..." -ForegroundColor Yellow

# Check Node.js
try {
    $nodeVersion = node --version
    Write-Host "  Node.js: $nodeVersion" -ForegroundColor Green
} catch {
    Write-Host "  Node.js: NOT FOUND" -ForegroundColor Red
    Write-Host "  Please install Node.js from https://nodejs.org" -ForegroundColor Yellow
    exit 1
}

# Check Rust
try {
    $rustVersion = rustc --version
    Write-Host "  Rust: $rustVersion" -ForegroundColor Green
} catch {
    Write-Host "  Rust: NOT FOUND" -ForegroundColor Red
    Write-Host "  Please install Rust from https://rustup.rs" -ForegroundColor Yellow
    exit 1
}

# Check Tauri CLI
try {
    $tauriVersion = npx tauri --version 2>&1
    Write-Host "  Tauri CLI: $tauriVersion" -ForegroundColor Green
} catch {
    Write-Host "  Tauri CLI: Installing..." -ForegroundColor Yellow
    npm install -g @tauri-apps/cli
}

Write-Host ""

# Install dependencies
Write-Host "Installing dependencies..." -ForegroundColor Yellow
Push-Location $PROJECT_ROOT
try {
    npm install
    Write-Host "  npm dependencies installed" -ForegroundColor Green
} catch {
    Write-Host "  Failed to install npm dependencies" -ForegroundColor Red
    exit 1
}

# Download Tesseract if not present
$tessExe = "$PROJECT_ROOT\src-tauri\binaries\tesseract-x86_64-pc-windows-msvc.exe"
if (-not (Test-Path $tessExe)) {
    Write-Host ""
    Write-Host "Downloading Tesseract OCR..." -ForegroundColor Yellow
    & "$PSScriptRoot\download_tesseract.ps1"
}

# Build the application
Write-Host ""
Write-Host "Building application..." -ForegroundColor Yellow
Write-Host "  This may take several minutes..." -ForegroundColor Gray

try {
    npm run tauri build
    Write-Host ""
    Write-Host "Build complete!" -ForegroundColor Green
} catch {
    Write-Host ""
    Write-Host "Build failed: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
} finally {
    Pop-Location
}

# Show output
Write-Host ""
Write-Host "========================================" -ForegroundColor Cyan
Write-Host "  Build Output" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

if (Test-Path $DIST_DIR) {
    Get-ChildItem $DIST_DIR -Filter "*.msi" | ForEach-Object {
        $sizeMB = [math]::Round($_.Length / 1MB, 2)
        Write-Host ""
        Write-Host "  MSI Installer: $($_.Name)" -ForegroundColor Green
        Write-Host "  Size: $sizeMB MB" -ForegroundColor White
        Write-Host "  Path: $($_.FullName)" -ForegroundColor Gray
    }
} else {
    Write-Host "  No MSI found. Check for NSIS installer instead." -ForegroundColor Yellow

    $nsisDir = "$PROJECT_ROOT\src-tauri\target\release\bundle\nsis"
    if (Test-Path $nsisDir) {
        Get-ChildItem $nsisDir -Filter "*.exe" | ForEach-Object {
            $sizeMB = [math]::Round($_.Length / 1MB, 2)
            Write-Host ""
            Write-Host "  NSIS Installer: $($_.Name)" -ForegroundColor Green
            Write-Host "  Size: $sizeMB MB" -ForegroundColor White
            Write-Host "  Path: $($_.FullName)" -ForegroundColor Gray
        }
    }
}

Write-Host ""
Write-Host "Done!" -ForegroundColor Cyan
