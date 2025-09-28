# Contributing to Plexify

Thank you for your interest in contributing to Plexify! This guide will help you get started with developing and contributing to this media transcoding CLI tool.

## Development Setup

### Prerequisites

- **Rust**: Version 1.70+ (install via [rustup.rs](https://rustup.rs/))
- **FFmpeg**: Required for media transcoding functionality
  - Ubuntu/Debian: `sudo apt install ffmpeg`
  - macOS: `brew install ffmpeg`
  - Windows: `winget install ffmpeg`

### Building from Source

```bash
git clone https://github.com/Weibye/plexify.git
cd plexify
cargo build
```

### Running Tests

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_scan_and_clean_workflow
```

### Running the Application

```bash
# Build and run directly
cargo run -- scan /path/to/media

# Or build release version
cargo build --release
./target/release/plexify scan /path/to/media
```

## Development Workflow

### Code Style

- Follow standard Rust formatting: `cargo fmt`
- Run Clippy for linting: `cargo clippy`
- Ensure all tests pass: `cargo test`
- Add documentation for public APIs

### Commit Messages

Use clear, descriptive commit messages:

```
Add support for additional video formats

- Extend MediaFileType enum with new formats
- Update FFmpeg command generation
- Add tests for new format handling
```

### Pull Request Process

1. Fork the repository
2. Create a feature branch: `git checkout -b feature/your-feature-name`
3. Make your changes with tests
4. Ensure all tests pass: `cargo test`
5. Run formatting: `cargo fmt`
6. Run linting: `cargo clippy`
7. Commit your changes
8. Push to your fork
9. Create a Pull Request

## Project Structure

```
src/
├── commands/     # CLI command implementations
│   ├── scan.rs   # Directory scanning logic
│   ├── work.rs   # Job processing logic
│   └── clean.rs  # Cleanup operations
├── config/       # Configuration management
├── job/          # Job definition and operations
├── queue/        # Job queue with atomic operations
├── ffmpeg/       # FFmpeg integration
├── worker/       # Worker coordination
└── main.rs       # CLI entry point
```

## Testing Guidelines

### Unit Tests

- Each module should have comprehensive unit tests
- Test both success and error cases
- Use temporary directories for file system tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_job_creation() {
        let job = Job::new(PathBuf::from("video.webm"), MediaFileType::WebM);
        assert_eq!(job.file_type, MediaFileType::WebM);
    }
}
```

### Integration Tests

- Test complete workflows using the CLI
- Use temporary directories and files
- Verify expected output and file creation

## Adding New Features

### Media Format Support

To add support for a new media format:

1. Extend `MediaFileType` enum in `src/job/mod.rs`
2. Update FFmpeg command generation in `src/ffmpeg/mod.rs`
3. Add file detection in `src/commands/scan.rs`
4. Add comprehensive tests

### New CLI Commands

To add a new CLI command:

1. Create a new module in `src/commands/`
2. Implement the command struct with `execute()` method
3. Add the command to the `Commands` enum in `src/main.rs`
4. Add integration tests

## Code Quality Standards

### Error Handling

- Use `anyhow::Result<T>` for error propagation
- Provide clear, actionable error messages
- Handle edge cases gracefully

```rust
pub async fn process_file(&self, path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(anyhow!("File does not exist: {}", path.display()));
    }
    // ... processing logic
}
```

### Logging

- Use `tracing` for structured logging
- Include relevant context in log messages
- Use appropriate log levels (error, warn, info, debug)

```rust
use tracing::{info, warn, error, debug};

info!("Starting scan for directory: {}", path.display());
warn!("Skipping file without subtitle: {}", file_path.display());
```

### Async/Await

- Use `tokio` for async operations
- Prefer async file operations for I/O
- Handle cancellation gracefully

## Performance Considerations

- Process files one at a time to avoid memory issues
- Use efficient file system operations
- Consider disk space and processing limits
- Test with large media files

## Documentation

- Add doc comments for public APIs
- Update README.md for user-facing changes
- Include examples in documentation
- Keep inline comments focused and helpful

## Security

- Validate all file paths
- Sanitize FFmpeg command arguments
- Handle file permissions properly
- Avoid command injection vulnerabilities

## Getting Help

- Check existing issues and discussions
- Review the documentation and code examples
- Ask questions in issue comments or discussions
- Follow the code of conduct

## License

By contributing to Plexify, you agree that your contributions will be licensed under the MIT License.