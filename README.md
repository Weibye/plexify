# Plexify

A simple, distributed media transcoding CLI tool that converts .webm and .mkv files to .mp4 format with subtitle support, optimized for Plex media servers.

## Features

- **Distributed Processing**: Queue-based system allows multiple workers to process jobs concurrently
- **Subtitle Support**: Handles external .vtt subtitles for .webm files and embedded subtitles for .mkv files
- **Background Processing**: Run workers in low-priority background mode
- **Configurable**: Customizable FFmpeg settings via environment variables
- **Atomic Job Processing**: Race condition-free job claiming for multiple workers
- **Signal Handling**: Graceful shutdown on SIGINT/SIGTERM

## Requirements

- **FFmpeg**: Required for media transcoding
- **Bash**: Version 4.0+ (for array support)
- **Standard Unix utilities**: `nice`, `ionice` (optional, for background processing)

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

**CentOS/RHEL:**
```bash
sudo yum install epel-release
sudo yum install ffmpeg
```

## Installation

1. Clone the repository:
```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
```

2. Make the script executable:
```bash
chmod +x plexify.sh
```

3. Optionally, add to your PATH:
```bash
sudo ln -s $(pwd)/plexify.sh /usr/local/bin/plexify
```

## Usage

### Basic Commands

```bash
# Scan a directory for media files and create transcoding jobs
./plexify.sh scan /path/to/media

# Process jobs from the queue (foreground)
./plexify.sh work /path/to/media

# Process jobs in background with low priority
./plexify.sh work /path/to/media --background

# Clean up temporary files
./plexify.sh clean /path/to/media
```

### Typical Workflow

1. **Scan**: Create jobs for all .webm and .mkv files in your media directory
```bash
./plexify.sh scan /home/user/Videos
```

2. **Work**: Start processing the queue (you can run multiple workers)
```bash
# Terminal 1 - High priority worker
./plexify.sh work /home/user/Videos

# Terminal 2 - Background worker
./plexify.sh work /home/user/Videos --background
```

3. **Monitor**: Check the `_worker.log` file for background worker progress

4. **Clean**: Remove temporary files when done
```bash
./plexify.sh clean /home/user/Videos
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
FFMPEG_PRESET="medium" FFMPEG_CRF="20" ./plexify.sh work /path/to/media
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
├── _completed/       # Finished jobs
└── _worker.log       # Background worker log
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
./plexify.sh work /media/videos

# Worker 2 (background, low priority)
./plexify.sh work /media/videos --background

# Worker 3 (on another machine with shared storage)
./plexify.sh work /shared/media/videos
```

Each worker atomically claims jobs to prevent conflicts.

## Signal Handling

Workers handle `SIGINT` (Ctrl+C) and `SIGTERM` gracefully:
- Completes current job before shutting down
- Returns job to queue if interrupted mid-processing
- Immediate shutdown if no job is currently running

## Troubleshooting

### Common Issues

1. **"No jobs found"**: 
   - Run `scan` command first
   - Check that .webm files have matching .vtt files

2. **FFmpeg errors**:
   - Verify FFmpeg is installed and in PATH
   - Check file permissions and disk space

3. **Permission errors**:
   - Ensure write permissions to media directory
   - Check that temporary directories can be created

4. **Background worker not starting**:
   - Check `_worker.log` for error messages
   - Verify `nohup` is available

### Debug Mode

Enable debug output by modifying the script:
```bash
# Uncomment this line at the top of plexify.sh
set -euo pipefail
```

## License

This project is open source. Please check the repository for license details.

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Test thoroughly
5. Submit a pull request

## Support

For issues and questions, please use the GitHub issue tracker.