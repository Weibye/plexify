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

### Option 1: From Source

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

### Option 2: Pre-built Binaries (Coming Soon)

Pre-built binaries for Linux, macOS, and Windows will be available in the GitHub releases.

## Usage

### Basic Commands

```bash
# Scan a directory for media files and create transcoding jobs
# Recursively scans all subdirectories for .webm and .mkv files
plexify scan /path/to/media

# Scan with a quality preset for consistent encoding settings
plexify scan --preset quality /path/to/media

# Process jobs from the queue (foreground)
plexify work /path/to/media

# Process jobs in background with low priority
plexify work /path/to/media --background

# Clean up temporary files
plexify clean /path/to/media

# Validate Plex naming scheme conformity
plexify validate /path/to/media
```

### Hierarchical Directory Support

Plexify automatically scans through your entire media directory hierarchy, finding media files in any subdirectory structure:

```
/media/
├── Movies/
│   ├── Action/
│   │   └── movie1.mkv
│   └── Comedy/
│       └── movie2.webm
│       └── movie2.vtt
├── TV Shows/
│   ├── Show1/
│   │   ├── Season 1/
│   │   │   └── episode1.webm
│   │   │   └── episode1.vtt
│   │   └── Season 2/
│   │       └── episode2.mkv
│   └── Show2/
│       └── episode.mkv
└── Documentaries/
    └── doc1.mkv
```

Running `plexify scan /media` will find and queue jobs for **all** media files regardless of their depth in the directory structure.

### Quality Presets

Plexify includes predefined quality presets for different use cases:

- **`fast`** - Fast encoding with good quality (veryfast/23/128k) - Default behavior
- **`balanced`** - Balanced encoding speed and quality (medium/20/192k) - Recommended
- **`quality`** - High quality, slower encoding (slow/18/256k) - Best for archival
- **`ultrafast`** - Ultra-fast encoding for quick previews (ultrafast/28/96k)
- **`archive`** - Archive quality for long-term storage (veryslow/15/320k)

Examples:
```bash
# Scan with balanced preset (recommended for most users)
plexify scan --preset balanced /path/to/media

# Scan with quality preset for best results
plexify scan --preset quality /path/to/media

# Scan with fast preset for quick transcoding
plexify scan --preset fast /path/to/media
```

Environment variables can override preset values:
```bash
# Use quality preset but override CRF to 20
FFMPEG_CRF=20 plexify scan --preset quality /path/to/media
```

### Typical Workflow

1. **Scan**: Create jobs for all .webm and .mkv files with your preferred quality preset
```bash
# Scan with balanced preset (recommended)
plexify scan --preset balanced /home/user/Videos

# Or scan with custom settings via environment variables
FFMPEG_PRESET=medium plexify scan /home/user/Videos
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

5. **Validate**: Check Plex naming scheme conformity (optional)
```bash
plexify validate /home/user/Videos
```

## Plex Naming Scheme Validation

The `validate` command checks your media files against Plex naming conventions and generates a detailed report:

```bash
plexify validate /path/to/media
```

### Supported Naming Patterns

**TV Shows:**
- `TV Shows/Show Name/Season NN/Show Name - sNNeNN - Episode Name.ext`
- `TV Shows/Show Name/Season NN/Show Name SNNeNN Episode Name.ext`  
- `TV Shows/Show Name/Season NN/SNNeNN - Episode Name.ext`

**Movies:**
- `Movies/Movie Name (Year)/Movie Name (Year).ext`
- `Movies/Collection Name/Movie Name (Year).ext`

### Example Output

```
📊 Plex Naming Scheme Validation Report
═══════════════════════════════════════
📂 Scanned directory: /home/user/Videos
📁 Files scanned: 12
⚠️  Issues found: 3

🔍 Issues Found:
─────────────────

❌ /home/user/Videos/random/movie.mkv
   Issue: File is not in a recognized directory structure (Movies/ or TV Shows/)

❌ /home/user/Videos/TV Shows/Show/episode.mkv
   Issue: TV show file doesn't match expected naming pattern

📈 Issue Summary:
─────────────────
• Directory Structure: 1 files
• TV Show Naming: 2 files
```

The report helps you identify files that need to be renamed for optimal Plex organization.

## Configuration

Plexify offers two ways to configure encoding settings:

### 1. Quality Presets (Recommended)
Use predefined presets for consistent, tested settings:
```bash
plexify scan --preset balanced /path/to/media  # Recommended for most users
plexify scan --preset quality /path/to/media   # Best quality
plexify scan --preset fast /path/to/media      # Fastest encoding
```

### 2. Environment Variables
Override individual settings or customize presets:
```bash
export FFMPEG_PRESET="veryfast"     # FFmpeg preset (default: veryfast)
export FFMPEG_CRF="23"              # Constant Rate Factor (default: 23)
export FFMPEG_AUDIO_BITRATE="128k"  # Audio bitrate (default: 128k)
export SLEEP_INTERVAL="60"          # Sleep between job checks in seconds (default: 60)
```

### Combining Presets and Environment Variables
Environment variables override preset values:
```bash
# Use quality preset but with faster encoding preset
FFMPEG_PRESET="medium" plexify scan --preset quality /path/to/media

# Use balanced preset but with higher quality CRF
FFMPEG_CRF="18" plexify scan --preset balanced /path/to/media
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

