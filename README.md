# Plexify

[![CI](https://github.com/Weibye/plexify/workflows/CI/badge.svg)](https://github.com/Weibye/plexify/actions/workflows/ci.yml)

A simple, distributed media transcoding CLI tool that converts .webm and .mkv files to .mp4 format with subtitle support, optimized for Plex media servers.

## Features

- **Distributed Processing**: Queue-based system allows multiple workers to process jobs concurrently
- **Subtitle Support**: Handles external .vtt subtitles for .webm files and embedded subtitles for .mkv files
- **Background Processing**: Run workers in low-priority background mode
- **Configurable**: Customizable FFmpeg settings via environment variables
- **Atomic Job Processing**: Race condition-free job claiming for multiple workers
- **Signal Handling**: Graceful shutdown on SIGINT/SIGTERM
- **Cross-Platform**: Works on Linux, macOS, and Windows
- **Modern Architecture**: Built with Rust for safety, performance, and maintainability

## Requirements

- **FFmpeg**: Required for media transcoding
- **Rust**: Version 1.70+ (for compilation) - pre-built binaries available

### Installing FFmpeg

**Ubuntu/Debian:**
```bash
sudo apt update
sudo apt install ffmpeg
```

**macOS:**
```bash
brew install ffmpeg
```

**Windows:**
```bash
winget install ffmpeg
```

**CentOS/RHEL:**
```bash
sudo yum install epel-release
sudo yum install ffmpeg
```

## Installation

### Option 1: Pre-built Binaries (Recommended)

Download the latest release from the [GitHub releases page](https://github.com/Weibye/plexify/releases):

1. **Download the binary for your platform:**
   - **Linux (x86_64)**: `plexify-linux-amd64`
   - **Linux (ARM64)**: `plexify-linux-arm64`
   - **Windows (x86_64)**: `plexify-windows-amd64.exe`
   - **macOS (Intel)**: `plexify-macos-amd64`
   - **macOS (Apple Silicon)**: `plexify-macos-arm64`

2. **Verify the download (recommended):**
   ```bash
   # Linux/macOS
   sha256sum -c plexify-*.sha256
   
   # Windows PowerShell
   Get-FileHash plexify-windows-amd64.exe -Algorithm SHA256
   ```

3. **Make executable (Linux/macOS only):**
   ```bash
   chmod +x plexify-*
   ```

4. **Move to system PATH (optional):**
   ```bash
   # Linux/macOS
   sudo mv plexify-* /usr/local/bin/plexify
   
   # Windows: Move to a directory in your PATH
   ```

### Option 2: From Source

1. Clone the repository:
```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
```

2. Build with Cargo:
```bash
cargo build --release
```

3. The binary will be available at `./target/release/plexify`

4. Optionally, install to system PATH:
```bash
cargo install --path .
```

### Keeping Systems Updated

#### Automated Updates (Recommended)

For production worker nodes, create an update script to automatically fetch the latest release:

**Linux/macOS (`update-plexify.sh`):**
```bash
#!/bin/bash
set -e

REPO="Weibye/plexify"
INSTALL_DIR="/usr/local/bin"
PLATFORM=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

# Map architecture names
case $ARCH in
  x86_64) ARCH="amd64" ;;
  aarch64|arm64) ARCH="arm64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

# Get latest release info
LATEST_RELEASE=$(curl -s "https://api.github.com/repos/$REPO/releases/latest")
VERSION=$(echo "$LATEST_RELEASE" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')
BINARY_NAME="plexify-${PLATFORM}-${ARCH}"

echo "Updating plexify to version $VERSION"

# Download binary and checksum
curl -L -o "/tmp/$BINARY_NAME" \
  "https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME"
curl -L -o "/tmp/$BINARY_NAME.sha256" \
  "https://github.com/$REPO/releases/download/$VERSION/$BINARY_NAME.sha256"

# Verify checksum
cd /tmp && sha256sum -c "$BINARY_NAME.sha256"

# Install
chmod +x "/tmp/$BINARY_NAME"
sudo mv "/tmp/$BINARY_NAME" "$INSTALL_DIR/plexify"

echo "Successfully updated plexify to $VERSION"
plexify --version || echo "plexify installed at $INSTALL_DIR/plexify"
```

**Windows PowerShell (`Update-Plexify.ps1`):**
```powershell
param(
    [string]$InstallDir = "$env:USERPROFILE\bin"
)

$ErrorActionPreference = "Stop"

$repo = "Weibye/plexify"
$binaryName = "plexify-windows-amd64.exe"

# Create install directory if it doesn't exist
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force
}

# Get latest release
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases/latest"
$version = $release.tag_name

Write-Host "Updating plexify to version $version"

# Download binary
$binaryUrl = "https://github.com/$repo/releases/download/$version/$binaryName"
$checksumUrl = "https://github.com/$repo/releases/download/$version/$binaryName.sha256"

$tempBinary = "$env:TEMP\$binaryName"
$tempChecksum = "$env:TEMP\$binaryName.sha256"

Invoke-WebRequest -Uri $binaryUrl -OutFile $tempBinary
Invoke-WebRequest -Uri $checksumUrl -OutFile $tempChecksum

# Verify checksum
$expectedHash = (Get-Content $tempChecksum).Split()[0]
$actualHash = (Get-FileHash $tempBinary -Algorithm SHA256).Hash

if ($expectedHash.ToUpper() -ne $actualHash.ToUpper()) {
    throw "Checksum verification failed"
}

# Install
$installPath = Join-Path $InstallDir "plexify.exe"
Move-Item $tempBinary $installPath -Force

Write-Host "Successfully updated plexify to $version"
Write-Host "Binary installed at: $installPath"
```

#### Manual Updates

1. Check the [releases page](https://github.com/Weibye/plexify/releases) for new versions
2. Download the appropriate binary for your platform
3. Replace your existing binary
4. Verify the installation: `plexify --version`

#### Automated Update Scripts

The repository includes ready-to-use update scripts:

**One-line update (Linux/macOS):**
```bash
curl -sSL https://raw.githubusercontent.com/Weibye/plexify/main/scripts/update-plexify.sh | bash
```

**Windows PowerShell:**
```powershell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Weibye/plexify/main/scripts/Update-Plexify.ps1" -OutFile "Update-Plexify.ps1"
.\Update-Plexify.ps1
```

See the [`scripts/`](scripts/) directory for detailed usage and automation options.

### Docker Deployment

For containerized worker nodes:

**Quick start:**
```bash
# Clone repository
git clone https://github.com/Weibye/plexify.git
cd plexify

# Set environment variables
export MEDIA_PATH=/path/to/your/media
export QUEUE_PATH=/path/to/shared/queue

# Start worker
docker-compose up plexify-worker

# Run scanner (in another terminal)
docker-compose run --rm plexify-scanner
```

**Production setup with multiple workers:**
```bash
# Start 3 background workers
docker-compose up -d --scale plexify-worker=3

# Run scanner periodically via cron
0 */6 * * * cd /path/to/plexify && docker-compose run --rm plexify-scanner
```

## Usage

### Basic Commands

```bash
# Scan a directory for media files and create transcoding jobs
plexify scan /path/to/media

# Process jobs from the queue (foreground)
plexify work /path/to/media

# Process jobs in background with low priority
plexify work /path/to/media --background

# Clean up temporary files
plexify clean /path/to/media
```

### Typical Workflow

1. **Scan**: Create jobs for all .webm and .mkv files in your media directory
```bash
plexify scan /home/user/Videos
```

2. **Work**: Start processing the queue (you can run multiple workers)
```bash
# Terminal 1 - High priority worker
plexify work /home/user/Videos

# Terminal 2 - Background worker
plexify work /home/user/Videos --background
```

3. **Monitor**: Check logs for progress (logs output to stdout)

4. **Clean**: Remove temporary files when done
```bash
plexify clean /home/user/Videos
```

## Configuration

Configure FFmpeg settings and behavior using environment variables:

### FFmpeg Settings
```bash
export FFMPEG_PRESET="veryfast"     # FFmpeg preset (default: veryfast)
export FFMPEG_CRF="23"              # Constant Rate Factor (default: 23)
export FFMPEG_AUDIO_BITRATE="128k"  # Audio bitrate (default: 128k)
export SLEEP_INTERVAL="60"          # Sleep between job checks in seconds (default: 60)
```

### Example with custom settings:
```bash
FFMPEG_PRESET="medium" FFMPEG_CRF="20" plexify work /path/to/media
```

## File Processing

### .webm Files
- Requires matching .vtt subtitle file (same name, different extension)
- Example: `video.webm` requires `video.vtt`
- Output: `video.mp4` with embedded subtitles

### .mkv Files
- Uses embedded subtitles from the source file
- Example: `video.mkv` → `video.mp4`
- Automatically maps first video, audio, and subtitle streams

## Directory Structure

Plexify creates temporary directories in your media root:

```
/path/to/media/
├── video1.webm
├── video1.vtt
├── video2.mkv
├── _queue/           # Pending jobs
├── _in_progress/     # Currently processing
└── _completed/       # Finished jobs
```

## FFmpeg Processing Details

### For .webm files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero \
  -i input.webm -i input.vtt \
  -map 0:v:0 -map 0:a:0 -map 1:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

### For .mkv files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero -fix_sub_duration \
  -i input.mkv \
  -map 0:v:0 -map 0:a:0 -map 0:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

## Distributed Processing

Multiple workers can safely process the same queue:

```bash
# Worker 1 (high priority)
plexify work /media/videos

# Worker 2 (background, low priority)
plexify work /media/videos --background

# Worker 3 (on another machine with shared storage)
plexify work /shared/media/videos
```

Each worker atomically claims jobs to prevent conflicts.

## Signal Handling

Workers handle `SIGINT` (Ctrl+C) and `SIGTERM` gracefully:
- Completes current job before shutting down
- Returns job to queue if interrupted mid-processing
- Immediate shutdown if no job is currently running

## Logging

The Rust version uses structured logging. Control log levels with the `RUST_LOG` environment variable:

```bash
# Default: info level
plexify work /path/to/media

# Debug level for troubleshooting
RUST_LOG=debug plexify work /path/to/media

# Only warnings and errors
RUST_LOG=warn plexify work /path/to/media
```

## Development

### Building from Source

```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
cargo build
```

### Running Tests

```bash
cargo test
```

### Code Quality

The project includes comprehensive CI/CD with:

- **Build & Test**: Automated testing on every PR and push to main
- **Code Formatting**: Enforced via `cargo fmt` 
- **Linting**: Code quality checks via `cargo clippy`
- **Security Audit**: Dependency vulnerability scanning via `cargo audit`

To run quality checks locally:
```bash
# Format code
cargo fmt

# Run linter 
cargo clippy --all-targets --all-features

# Check formatting
cargo fmt --all -- --check
```

### Code Structure

The project is organized into modules:

- `commands/` - CLI command implementations (scan, work, clean)
- `config/` - Configuration management
- `job/` - Job definitions and processing logic
- `queue/` - Job queue management with atomic operations
- `ffmpeg/` - FFmpeg integration
- `worker/` - Worker coordination (extensible for future features)

## Troubleshooting

### Common Issues

1. **"No jobs found"**: 
   - Run `scan` command first
   - Check that .webm files have matching .vtt files

2. **FFmpeg errors**:
   - Verify FFmpeg is installed and in PATH
   - Check file permissions and disk space
   - Enable debug logging: `RUST_LOG=debug plexify work /path`

3. **Permission errors**:
   - Ensure write permissions to media directory
   - Check that temporary directories can be created

### Debug Mode

Enable debug output:
```bash
RUST_LOG=debug plexify scan /path/to/media
```

