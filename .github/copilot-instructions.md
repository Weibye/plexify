# Copilot Instructions for Plexify

## Project Overview

Plexify is a simple, distributed media transcoding CLI tool written in Rust that converts .webm and .mkv files to .mp4 format with subtitle support, specifically optimized for Plex media servers.

### Key Features
- **Distributed Processing**: Queue-based system for concurrent job processing
- **Subtitle Support**: External .vtt subtitles for .webm, embedded subtitles for .mkv
- **Background Processing**: Low-priority background worker mode
- **Configurable FFmpeg**: Environment variable configuration
- **Atomic Job Processing**: Race condition-free job claiming
- **Signal Handling**: Graceful shutdown support

## Architecture

The project follows a modular structure:

```
src/
├── commands/     # CLI command implementations (scan, work, clean)
├── config/      # Configuration management via environment variables
├── job/         # Job definitions and processing logic
├── queue/       # Job queue management with atomic operations
├── ffmpeg/      # FFmpeg integration and command generation
└── worker/      # Worker coordination (extensible for future features)
```

## Development Guidelines

### Rust-Specific Patterns

1. **Error Handling**: Use `anyhow::Result<T>` for error propagation
2. **Async/Await**: All I/O operations use `tokio` for async execution
3. **Logging**: Use `tracing` crate with structured logging
4. **Configuration**: Environment variables loaded through `Config::from_env()`
5. **Serialization**: Jobs use `serde` for JSON serialization/deserialization

### Code Quality Standards

1. **Testing**: Every module should have comprehensive unit tests
2. **Documentation**: Public APIs must have doc comments
3. **Error Messages**: Provide clear, actionable error messages for users
4. **Performance**: Consider memory usage for large media file operations
5. **Safety**: Use `PathBuf` for all file system operations

### Quality Checks - MANDATORY BEFORE FINISHING

**ALL agents MUST run these quality checks before finishing any work and ensure they pass:**

1. **Build Check**: `cargo build` - Must succeed without errors
2. **Test Check**: `cargo test` - All tests must pass
3. **Format Check**: `cargo fmt --all -- --check` - Code must be properly formatted
4. **Clippy Check**: `cargo clippy --all-targets --all-features` - Must pass without errors (warnings are acceptable)

**If any check fails, the agent MUST fix the issues before completing the task.**

These checks mirror the CI/CD pipeline and ensure code quality is maintained. The agent should run these checks:
- After making any code changes
- Before using the report_progress tool to finalize work
- If checks fail, fix issues and re-run all checks until they pass

### CI/CD Pipeline

The repository includes a comprehensive CI/CD pipeline (`.github/workflows/ci.yml`) with the following jobs:

1. **Test Job**: Builds and runs all tests (`cargo build && cargo test`)
2. **Format Job**: Validates code formatting (`cargo fmt --all -- --check`)
3. **Clippy Job**: Runs linting checks (`cargo clippy --all-targets --all-features`)
4. **Security Audit**: Scans dependencies for vulnerabilities using `cargo audit`
5. **All Checks Job**: Ensures all above jobs pass before allowing merges

**All jobs must pass for PRs to be merged.** Agents must ensure their changes pass these same checks locally.

### Pull Request Standards
1. Any transient information, like feature x is now faster, feature y has changed, etc. belongs to the PR description and the changelog, and only those two placed. Code, documentation, and tests must be self-explanatory and not contain any transient information. They should be valid for the lifetime of the code and reflect the current state of the code.
2. Pull requests should be short, succinct, and focused. No need to repeat unnecessary information.

### Media Processing Specifics

1. **FFmpeg Integration**: 
   - Always use configured presets and quality settings
   - Handle both .webm (external subtitles) and .mkv (embedded subtitles)
   - Ensure atomic file operations (temp files, then rename)

2. **Job Processing**:
   - Jobs are identified by UUID
   - Queue operations must be atomic to prevent race conditions
   - Support graceful shutdown with signal handling

3. **File System Operations**:
   - Use relative paths within media directory
   - Check for existing output files to avoid duplicate work
   - Validate subtitle file existence for .webm files

### FFmpeg Command Patterns

For .webm files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero \
  -i input.webm -i input.vtt \
  -map 0:v:0 -map 0:a:0 -map 1:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

For .mkv files:
```bash
ffmpeg -fflags +genpts -avoid_negative_ts make_zero -fix_sub_duration \
  -i input.mkv \
  -map 0:v:0 -map 0:a:0 -map 0:s:0 \
  -c:v libx264 -preset veryfast -crf 23 \
  -c:a aac -b:a 128k \
  -c:s mov_text \
  -y output.mp4
```

## Environment Configuration

Key environment variables:
- `FFMPEG_PRESET`: FFmpeg encoding preset (default: "veryfast")
- `FFMPEG_CRF`: Constant Rate Factor (default: "23")
- `FFMPEG_AUDIO_BITRATE`: Audio bitrate (default: "128k")
- `SLEEP_INTERVAL`: Sleep between job checks in seconds (default: 60)
- `RUST_LOG`: Logging level (use "debug" for detailed output)

## Common Patterns

### Job Creation
```rust
let quality_settings = QualitySettings::from_env();
let post_processing = PostProcessingSettings::default();

let job = Job::new(
    relative_path,
    MediaFileType::WebM,
    quality_settings,
    post_processing,
);

if !job.output_exists(Some(&media_root)) && job.has_required_subtitle(Some(&media_root))? {
    queue.enqueue_job(&job).await?;
}
```

### Configuration Loading
```rust
let config = Config::from_env();
```

### Error Handling
```rust
if let Err(e) = result {
    error!("Operation failed: {}", e);
    return Err(e.into());
}
```

## Testing Guidelines

1. **Unit Tests**: Test business logic in isolation
2. **Integration Tests**: Test CLI commands end-to-end using temporary directories
3. **Mock External Dependencies**: Use temp directories for file system tests
4. **Error Cases**: Test error conditions and edge cases
5. **Async Tests**: Use `#[tokio::test]` for async test functions

### Local Quality Checks

Before submitting any changes, run these commands locally to ensure code quality:

```bash
# Format code (auto-fix)
cargo fmt

# Build and check for errors
cargo build

# Run all tests
cargo test

# Check code formatting (must pass)
cargo fmt --all -- --check

# Run linter (must pass)
cargo clippy --all-targets --all-features
```

**Note**: The `cargo fmt --all -- --check` and `cargo clippy` commands must pass without errors for CI to succeed.

## CLI Usage Patterns

The tool supports three main commands:

1. **Scan**: `plexify scan /path/to/media` - Discover media files and create jobs
2. **Work**: `plexify work /path/to/media` - Process jobs from the queue
3. **Clean**: `plexify clean /path/to/media` - Remove temporary files

## Performance Considerations

1. **Memory Usage**: Process files one at a time to avoid memory issues with large media files
2. **CPU Usage**: Use background mode (`--background`) for low-priority processing
3. **Disk I/O**: Minimize file system operations through efficient job queuing
4. **Concurrency**: Support multiple workers through atomic file-based job claiming

## Security Considerations

1. **Path Traversal**: Always validate file paths are within the media directory
2. **Command Injection**: Sanitize FFmpeg arguments properly
3. **File Permissions**: Handle permission errors gracefully
4. **Resource Limits**: Consider disk space and processing limits

## Debugging Tips

1. **Enable Debug Logging**: `RUST_LOG=debug plexify command args`
2. **Check FFmpeg Installation**: Ensure FFmpeg is in PATH
3. **Validate File Permissions**: Ensure write access to media directory
4. **Monitor Job Queue**: Check `_queue/` directory for pending jobs