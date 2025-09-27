# Plexify

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

