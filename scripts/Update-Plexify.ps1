# Plexify Update Script for Windows
# Downloads and installs the latest release of plexify

param(
    [string]$InstallDir = "$env:USERPROFILE\bin",
    [switch]$Force,
    [switch]$Help
)

if ($Help) {
    Write-Host @"
Plexify Update Script for Windows

Downloads and installs the latest release of plexify from GitHub.

USAGE:
    .\Update-Plexify.ps1 [-InstallDir <path>] [-Force] [-Help]

PARAMETERS:
    -InstallDir     Directory to install plexify (default: $env:USERPROFILE\bin)
    -Force          Force installation even if latest version is already installed
    -Help           Show this help message

EXAMPLES:
    .\Update-Plexify.ps1
    .\Update-Plexify.ps1 -InstallDir "C:\Program Files\plexify"
    .\Update-Plexify.ps1 -Force

REQUIREMENTS:
    - PowerShell 5.0 or higher
    - Internet connection
    - Write permissions to the installation directory
"@
    exit 0
}

$ErrorActionPreference = "Stop"

$repo = "Weibye/plexify"
$binaryName = "plexify-windows-amd64.exe"

Write-Host "Plexify Update Script" -ForegroundColor Blue
Write-Host "==============================" -ForegroundColor Blue

# Check PowerShell version
if ($PSVersionTable.PSVersion.Major -lt 5) {
    Write-Host "Error: PowerShell 5.0 or higher is required" -ForegroundColor Red
    exit 1
}

# Get current version if plexify is installed
$currentVersion = ""
try {
    $plexifyPath = Get-Command plexify -ErrorAction SilentlyContinue
    if ($plexifyPath) {
        $versionOutput = & plexify --version 2>$null
        if ($versionOutput -match "plexify (\S+)") {
            $currentVersion = $matches[1]
            Write-Host "Current version: $currentVersion" -ForegroundColor Yellow
        }
    } else {
        Write-Host "plexify not found in PATH" -ForegroundColor Yellow
    }
} catch {
    Write-Host "Could not determine current version" -ForegroundColor Yellow
}

# Create install directory if it doesn't exist
if (!(Test-Path $InstallDir)) {
    Write-Host "Creating install directory: $InstallDir"
    try {
        New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    } catch {
        Write-Host "Error: Failed to create install directory: $_" -ForegroundColor Red
        exit 1
    }
}

# Check write permissions
$testFile = Join-Path $InstallDir "test-write-permissions.tmp"
try {
    [System.IO.File]::WriteAllText($testFile, "test")
    Remove-Item $testFile -Force
} catch {
    Write-Host "Error: No write permissions to $InstallDir" -ForegroundColor Red
    Write-Host "Try running as Administrator or choose a different directory" -ForegroundColor Red
    exit 1
}

# Get latest release
Write-Host "Fetching latest release information..."
try {
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
} catch {
    Write-Host "Error: Failed to fetch release information: $_" -ForegroundColor Red
    exit 1
}

$version = $release.tag_name
if (-not $version) {
    Write-Host "Error: Could not determine latest version" -ForegroundColor Red
    exit 1
}

Write-Host "Latest version: $version" -ForegroundColor Green

# Check if we're already up to date
$versionWithoutV = $version -replace '^v', ''
if ($currentVersion -eq $versionWithoutV -and -not $Force) {
    Write-Host "You already have the latest version installed!" -ForegroundColor Green
    Write-Host "Use -Force to reinstall anyway" -ForegroundColor Yellow
    exit 0
}

Write-Host "Downloading plexify $version for Windows..."

# Download binary
$binaryUrl = "https://github.com/$repo/releases/download/$version/$binaryName"
$checksumUrl = "https://github.com/$repo/releases/download/$version/$binaryName.sha256"

$tempDir = [System.IO.Path]::GetTempPath()
$tempBinary = Join-Path $tempDir $binaryName
$tempChecksum = Join-Path $tempDir "$binaryName.sha256"

Write-Host "Downloading binary..."
try {
    Invoke-WebRequest -Uri $binaryUrl -OutFile $tempBinary -UseBasicParsing
} catch {
    Write-Host "Error: Failed to download binary: $_" -ForegroundColor Red
    exit 1
}

Write-Host "Downloading checksum..."
try {
    Invoke-WebRequest -Uri $checksumUrl -OutFile $tempChecksum -UseBasicParsing
} catch {
    Write-Host "Error: Failed to download checksum: $_" -ForegroundColor Red
    exit 1
}

# Verify checksum
Write-Host "Verifying checksum..."
try {
    $expectedHash = (Get-Content $tempChecksum -Raw).Split()[0].Trim()
    $actualHash = (Get-FileHash $tempBinary -Algorithm SHA256).Hash
    
    if ($expectedHash.ToUpper() -ne $actualHash.ToUpper()) {
        Write-Host "Error: Checksum verification failed" -ForegroundColor Red
        Write-Host "Expected: $expectedHash" -ForegroundColor Red
        Write-Host "Actual:   $actualHash" -ForegroundColor Red
        exit 1
    }
    Write-Host "Checksum verified successfully" -ForegroundColor Green
} catch {
    Write-Host "Error: Failed to verify checksum: $_" -ForegroundColor Red
    exit 1
}

# Install
$installPath = Join-Path $InstallDir "plexify.exe"
Write-Host "Installing to: $installPath"

try {
    Move-Item $tempBinary $installPath -Force
} catch {
    Write-Host "Error: Failed to install binary: $_" -ForegroundColor Red
    exit 1
}

# Clean up temporary checksum file
try {
    Remove-Item $tempChecksum -Force -ErrorAction SilentlyContinue
} catch {
    # Ignore cleanup errors
}

Write-Host "Successfully updated plexify to $version" -ForegroundColor Green

# Verify installation
try {
    $installedVersion = & $installPath --version 2>$null
    if ($installedVersion -match "plexify (\S+)") {
        $actualVersion = $matches[1]
        if ($actualVersion -eq $versionWithoutV) {
            Write-Host "Installation verified: plexify $actualVersion" -ForegroundColor Green
        } else {
            Write-Host "Warning: Version mismatch after installation" -ForegroundColor Yellow
            Write-Host "Expected: $versionWithoutV, Got: $actualVersion" -ForegroundColor Yellow
        }
    }
} catch {
    Write-Host "Warning: Could not verify installation" -ForegroundColor Yellow
}

Write-Host "Binary installed at: $installPath" -ForegroundColor Cyan

# Check if install directory is in PATH
$pathDirs = $env:PATH -split ';'
if ($pathDirs -notcontains $InstallDir) {
    Write-Host ""
    Write-Host "Note: $InstallDir is not in your PATH" -ForegroundColor Yellow
    Write-Host "To add it permanently, run:" -ForegroundColor Yellow
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', `$env:Path + ';$InstallDir', 'User')" -ForegroundColor Cyan
    Write-Host "Or use the full path: $installPath" -ForegroundColor Cyan
}

Write-Host ""
Write-Host "Update complete! You can now use the updated plexify." -ForegroundColor Green