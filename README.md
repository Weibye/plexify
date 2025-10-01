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

# Process jobs with episode prioritization (series episodes first, in order)
plexify work /path/to/media --priority episode

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

### Episode Prioritization

Plexify supports intelligent job prioritization for TV series episodes:

```bash
# Process jobs with episode prioritization
plexify work /path/to/media --priority episode

# Default behavior - process jobs in order found
plexify work /path/to/media --priority none  # or just omit --priority
```

**Episode Priority Mode:**
- **Series episodes are processed first**, sorted alphabetically by series name
- **Within each series**, episodes are processed in ascending order (S01E01, S01E02, S01E03...)
- **Non-episode content** (movies, etc.) is processed after all episodes
- **Perfect for binge-watching scenarios** - get your episodes in the right order

**Example processing order with `--priority episode`:**
1. Series/Better Call Saul/Season 01/Better Call Saul S01E01 Uno.mkv
2. Series/Better Call Saul/Season 01/Better Call Saul S01E02 Mijo.mkv  
3. Series/Breaking Bad/Season 01/Breaking Bad S01E01 Pilot.mkv
4. Series/Breaking Bad/Season 01/Breaking Bad S01E03 Gray Matter.mkv
5. Movies/The Matrix (1999)/The Matrix (1999).mkv

**Supported episode formats:**
- `Series/Show Name/Season XX/Show Name SxxExx Episode Title.ext`
- `Series/Show Name {tvdb-12345}/Season XX/Show Name SxxExx Episode Title.ext`
- `Series/Show Name/Season XX - Extra Info/Show Name SxxExx Episode Title.ext`
- `Anime/Show Name/Season XX/Show Name SxxExx Episode Title.ext`

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

### .plexifyignore Support

Plexify supports `.plexifyignore` files to exclude directories and files from scanning and validation. These files work similar to `.gitignore` files and can be placed at any level in your directory tree.

#### Pattern Syntax

- **Basic patterns**: `filename.ext`, `directory_name`
- **Wildcards**: `*.tmp`, `*.log` 
- **Directory patterns**: `Downloads/` (trailing slash matches directories only)
- **Negation**: `!important.mkv` (include files that would otherwise be ignored)
- **Path patterns**: `path/to/file` (relative to the .plexifyignore location)
- **Root patterns**: `/Downloads` (absolute from the .plexifyignore location)

#### Example .plexifyignore

```
# Ignore system directories
Downloads/
InProgress/
lost+found/
tools/

# Ignore temporary and backup files
*.tmp
*.bak
*.old
*.DS_Store
Thumbs.db

# Ignore specific directories but allow important files
old_episodes/
!important_episode.mkv

# Ignore files in root only
/temp_file.mkv
```

#### Usage

1. Create a `.plexifyignore` file in your media root or any subdirectory
2. Add patterns for files/directories you want to exclude
3. Run `plexify scan` or `plexify validate` - ignored paths will be skipped automatically

The `scan` and `validate` commands will show how many paths were ignored:

```
📋 Ignored 15 paths due to .plexifyignore patterns
```

**Note**: Nested `.plexifyignore` files are supported - patterns from parent directories apply to child directories, with child patterns taking precedence.

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

**TV Series:**
- `Series/Show Name/Season NN/Show Name - sNNeNN - Episode Name.ext`
- `Series/Show Name/Season NN/Show Name SNNeNN Episode Name.ext`  
- `Series/Show Name/Season NN/SNNeNN - Episode Name.ext`
- `Series/Show Name {tvdb-XXXXXX}/Season NN/Show Name SNNeNN Episode Name.ext` (with TVDB id)
- `Series/Show Name {tvdb-XXXXXX}/Season NN - Arc Name/Show Name - SNNeNN - Episode Name.ext` (with extended season name)

**Anime:**
- `Anime/Show Name/Season NN/Show Name - sNNeNN - Episode Name.ext`
- `Anime/Show Name/Season NN/Show Name SNNeNN Episode Name.ext`
- `Anime/Show Name {tvdb-XXXXXX}/Season NN/Show Name SNNeNN Episode Name.ext` (with TVDB id)

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

