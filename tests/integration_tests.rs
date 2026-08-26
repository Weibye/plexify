use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

use serial_test::serial;

use plexify::queue::JobQueue;
use plexify::JobPriority;

/// Path to the binary under test.
///
/// Cargo builds the binary before running integration tests and supplies its path
/// at compile time. Tests must not run `cargo build` themselves: the outer
/// `cargo test` already holds the build lock, so a nested build fails and every
/// later test fails behind it.
const PLEXIFY_BIN: &str = env!("CARGO_BIN_EXE_plexify");

/// Test the complete scan -> clean workflow
#[test]
#[serial]
fn test_scan_and_clean_workflow() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test files
    fs::write(temp_path.join("video1.webm"), "").unwrap();
    fs::write(temp_path.join("video1.vtt"), "").unwrap();
    fs::write(temp_path.join("video2.mkv"), "").unwrap();
    fs::write(temp_path.join("video3.webm"), "").unwrap(); // No .vtt file

    // Test scan command (use temp_path as both media dir and queue dir)
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    let scan_stdout = String::from_utf8_lossy(&scan_output.stdout);
    let scan_stderr = String::from_utf8_lossy(&scan_output.stderr);
    let scan_output_text = format!("{scan_stdout}{scan_stderr}");

    assert!(
        scan_output_text.contains("Added 2 new jobs"),
        "Expected 2 jobs to be created, got: {scan_output_text}"
    );
    assert!(
        scan_output_text.contains("SKIPPING: Missing subtitle file"),
        "Expected video3.webm to be skipped, got: {scan_output_text}"
    );

    // Verify queue files were created
    assert!(temp_path.join("_queue").exists());

    // Check that job files were created (they will have UUID names now)
    let queue_dir = temp_path.join("_queue");
    let job_files: Vec<_> = std::fs::read_dir(&queue_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "job" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(job_files.len(), 2, "Expected 2 job files to be created");

    // Check that video3.webm was not processed (no matching .vtt file)
    assert!(
        scan_output_text.contains("SKIPPING: Missing subtitle file"),
        "Expected video3.webm to be skipped, got: {scan_output_text}"
    );

    // Test clean command
    let clean_output = Command::new(PLEXIFY_BIN)
        .args([
            "clean",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute clean command");

    assert!(clean_output.status.success(), "Clean command failed");

    // Verify queue directories were removed
    assert!(!temp_path.join("_queue").exists());
    assert!(!temp_path.join("_in_progress").exists());
    assert!(!temp_path.join("_completed").exists());
}

/// Test help commands work
#[test]
#[serial]
fn test_help_commands() {
    let help_output = Command::new(PLEXIFY_BIN)
        .arg("--help")
        .output()
        .expect("Failed to execute help command");

    assert!(help_output.status.success(), "Help command failed");

    let help_stdout = String::from_utf8_lossy(&help_output.stdout);
    assert!(
        help_stdout.contains("plexify"),
        "Help should contain program name"
    );
    assert!(
        help_stdout.contains("scan"),
        "Help should list scan command"
    );
    assert!(
        help_stdout.contains("work"),
        "Help should list work command"
    );
    assert!(
        help_stdout.contains("clean"),
        "Help should list clean command"
    );
}

/// Test that invalid paths are handled gracefully
#[test]
#[serial]
fn test_invalid_paths() {
    // Test scan with non-existent directory
    let scan_output = Command::new(PLEXIFY_BIN)
        .args(["scan", "/non/existent/path"])
        .output()
        .expect("Failed to execute scan command");

    assert!(
        !scan_output.status.success(),
        "Scan should fail with invalid path"
    );

    // Test work with non-existent directory
    let work_output = Command::new(PLEXIFY_BIN)
        .args(["work", "/non/existent/path"])
        .output()
        .expect("Failed to execute work command");

    assert!(
        !work_output.status.success(),
        "Work should fail with invalid path"
    );

    // Test clean with non-existent directory
    let clean_output = Command::new(PLEXIFY_BIN)
        .args(["clean", "/non/existent/path"])
        .output()
        .expect("Failed to execute clean command");

    assert!(
        !clean_output.status.success(),
        "Clean should fail with invalid path"
    );
}

/// Test that job files contain all work details
#[test]
#[serial]
fn test_job_files_contain_complete_details() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test files
    fs::write(temp_path.join("video1.webm"), "fake webm content").unwrap();
    fs::write(
        temp_path.join("video1.vtt"),
        "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nTest subtitle",
    )
    .unwrap();
    fs::write(temp_path.join("video2.mkv"), "fake mkv content").unwrap();

    // Set custom environment variables to test they're captured
    // Save current values first
    let original_preset = std::env::var("FFMPEG_PRESET").ok();
    let original_crf = std::env::var("FFMPEG_CRF").ok();
    let original_bitrate = std::env::var("FFMPEG_AUDIO_BITRATE").ok();

    std::env::set_var("FFMPEG_PRESET", "fast");
    std::env::set_var("FFMPEG_CRF", "20");
    std::env::set_var("FFMPEG_AUDIO_BITRATE", "192k");

    // Run scan command (use temp_path as both media dir and queue dir)
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    // Read and verify job files
    let queue_dir = temp_path.join("_queue");
    assert!(queue_dir.exists());

    let job_files: Vec<_> = std::fs::read_dir(&queue_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "job" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(job_files.len(), 2, "Expected 2 job files to be created");

    // Read and parse job files to verify they contain all details
    for job_file in job_files {
        let content = fs::read_to_string(&job_file).unwrap();
        let job: serde_json::Value = serde_json::from_str(&content).unwrap();

        // Verify required fields are present
        assert!(job.get("id").is_some(), "Job should have id field");
        assert!(
            job.get("input_path").is_some(),
            "Job should have input_path field"
        );
        assert!(
            job.get("output_path").is_some(),
            "Job should have output_path field"
        );
        assert!(
            job.get("file_type").is_some(),
            "Job should have file_type field"
        );

        // Verify quality settings are captured from environment
        let quality_settings = job.get("quality_settings").unwrap();
        assert_eq!(quality_settings.get("ffmpeg_preset").unwrap(), "fast");
        assert_eq!(quality_settings.get("ffmpeg_crf").unwrap(), "20");
        assert_eq!(
            quality_settings.get("ffmpeg_audio_bitrate").unwrap(),
            "192k"
        );

        // Verify post-processing settings
        let post_processing = job.get("post_processing").unwrap();
        assert_eq!(post_processing.get("disable_source_files").unwrap(), true);

        // Verify paths are consistent
        let input_path = job.get("input_path").unwrap().as_str().unwrap();
        let output_path = job.get("output_path").unwrap().as_str().unwrap();

        if input_path.ends_with(".webm") {
            assert!(output_path.ends_with(".mp4"));
            assert!(job
                .get("subtitle_path")
                .unwrap()
                .as_str()
                .unwrap()
                .ends_with(".vtt"));
        } else if input_path.ends_with(".mkv") {
            assert!(output_path.ends_with(".mp4"));
            assert!(job.get("subtitle_path").unwrap().is_null());
        }
    }

    // Restore original environment variables
    match original_preset {
        Some(val) => std::env::set_var("FFMPEG_PRESET", val),
        None => std::env::remove_var("FFMPEG_PRESET"),
    }
    match original_crf {
        Some(val) => std::env::set_var("FFMPEG_CRF", val),
        None => std::env::remove_var("FFMPEG_CRF"),
    }
    match original_bitrate {
        Some(val) => std::env::set_var("FFMPEG_AUDIO_BITRATE", val),
        None => std::env::remove_var("FFMPEG_AUDIO_BITRATE"),
    }
}

/// Test complete workflow including work folder functionality
#[test]
#[serial]
fn test_work_folder_workflow() {
    let temp_dir = TempDir::new().unwrap();
    let media_path = temp_dir.path().join("media");
    let work_path = temp_dir.path().join("work");

    fs::create_dir_all(&media_path).unwrap();
    fs::create_dir_all(&work_path).unwrap();

    // Create test media files
    fs::write(media_path.join("test_video.mkv"), "fake mkv content").unwrap();

    // Set environment variable for faster processing
    std::env::set_var("FFMPEG_PRESET", "ultrafast");

    // First, scan to create jobs
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            media_path.to_str().unwrap(),
            "--work-dir",
            work_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run scan command");

    assert!(
        scan_output.status.success(),
        "Scan command failed: {}",
        String::from_utf8_lossy(&scan_output.stderr)
    );

    // Check that job files were created
    let queue_dir = work_path.join("_queue");
    assert!(queue_dir.exists(), "Queue directory should exist");

    let mut job_files = Vec::new();
    for entry in fs::read_dir(&queue_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().unwrap_or_default() == "job" {
            job_files.push(entry.path());
        }
    }
    assert!(
        !job_files.is_empty(),
        "Should have created at least one job file"
    );

    // Verify job contains the expected settings
    let job_content = fs::read_to_string(&job_files[0]).unwrap();
    let job_json: serde_json::Value = serde_json::from_str(&job_content).unwrap();
    let post_processing = job_json.get("post_processing").unwrap();
    assert_eq!(post_processing.get("disable_source_files").unwrap(), true);

    // Note: We can't actually test the work command with a real FFmpeg conversion
    // in CI because FFmpeg might not be available, but we've verified:
    // 1. Jobs are created with the correct work folder settings
    // 2. Unit tests verify the work folder logic
    // 3. Integration tests verify the complete scan workflow

    // Clean up environment variable
    std::env::remove_var("FFMPEG_PRESET");
}

/// Test the add command with individual files
#[test]
#[serial]
fn test_add_command_individual_files() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test files
    let mkv_file = temp_path.join("movie.mkv");
    let webm_file = temp_path.join("video.webm");
    let vtt_file = temp_path.join("video.vtt");

    fs::write(&mkv_file, "test mkv content").unwrap();
    fs::write(&webm_file, "test webm content").unwrap();
    fs::write(&vtt_file, "test vtt content").unwrap();

    // Test add command with MKV (no subtitles needed)
    let add_mkv_output = Command::new(PLEXIFY_BIN)
        .args([
            "add",
            mkv_file.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute add command for MKV");

    assert!(add_mkv_output.status.success(), "Add MKV command failed");
    let add_mkv_stdout = String::from_utf8_lossy(&add_mkv_output.stdout);
    assert!(
        add_mkv_stdout.contains("Successfully created transcoding job"),
        "Should create job for MKV file"
    );

    // Test add command with WebM (with subtitles)
    let add_webm_output = Command::new(PLEXIFY_BIN)
        .args([
            "add",
            webm_file.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
            "--preset",
            "quality",
        ])
        .output()
        .expect("Failed to execute add command for WebM");

    assert!(add_webm_output.status.success(), "Add WebM command failed");
    let add_webm_stdout = String::from_utf8_lossy(&add_webm_output.stdout);
    assert!(
        add_webm_stdout.contains("Successfully created transcoding job"),
        "Should create job for WebM file with subtitles"
    );

    // Verify queue directory and job files were created
    let queue_dir = temp_path.join("_queue");
    assert!(queue_dir.exists(), "Queue directory should exist");

    let job_count = fs::read_dir(&queue_dir)
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
        .count();
    assert_eq!(job_count, 2, "Should have created 2 job files");

    // Test error case: WebM without subtitle
    let webm_no_sub = temp_path.join("nosub.webm");
    fs::write(&webm_no_sub, "webm without sub").unwrap();

    let add_nosub_output = Command::new(PLEXIFY_BIN)
        .args([
            "add",
            webm_no_sub.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute add command for WebM without subtitle");

    assert!(
        !add_nosub_output.status.success(),
        "Add command should fail for WebM without subtitle"
    );

    // Test error case: unsupported file type
    let mp4_file = temp_path.join("video.mp4");
    fs::write(&mp4_file, "mp4 content").unwrap();

    let add_mp4_output = Command::new(PLEXIFY_BIN)
        .args([
            "add",
            mp4_file.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute add command for unsupported file");

    assert!(
        !add_mp4_output.status.success(),
        "Add command should fail for unsupported file type"
    );
}

/// Test hierarchical directory scanning functionality
#[test]
#[serial]
fn test_hierarchical_directory_scanning() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create hierarchical directory structure
    fs::create_dir_all(temp_path.join("Movies/Action")).unwrap();
    fs::create_dir_all(temp_path.join("Movies/Comedy")).unwrap();
    fs::create_dir_all(temp_path.join("TV Shows/Show1/Season 1")).unwrap();
    fs::create_dir_all(temp_path.join("TV Shows/Show1/Season 2")).unwrap();
    fs::create_dir_all(temp_path.join("TV Shows/Show2")).unwrap();

    // Create media files in different subdirectories
    fs::write(temp_path.join("Movies/Action/action1.mkv"), "").unwrap();
    fs::write(temp_path.join("Movies/Comedy/comedy1.webm"), "").unwrap();
    fs::write(temp_path.join("Movies/Comedy/comedy1.vtt"), "").unwrap();
    fs::write(temp_path.join("TV Shows/Show1/Season 1/episode1.webm"), "").unwrap();
    fs::write(temp_path.join("TV Shows/Show1/Season 1/episode1.vtt"), "").unwrap();
    fs::write(temp_path.join("TV Shows/Show1/Season 2/episode2.mkv"), "").unwrap();
    fs::write(temp_path.join("TV Shows/Show2/episode.mkv"), "").unwrap();

    // Run scan command
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    let scan_stdout = String::from_utf8_lossy(&scan_output.stdout);
    let scan_stderr = String::from_utf8_lossy(&scan_output.stderr);
    let scan_output_text = format!("{scan_stdout}{scan_stderr}");
    // Normalize all path separators (backslash or multiple slashes) to a single '/'
    let scan_output_text = scan_output_text.replace('\\', "/").replace("//", "/");

    // Verify that it mentions recursive scanning
    assert!(
        scan_output_text.contains("Recursively scanning all subdirectories"),
        "Should mention recursive scanning, got: {scan_output_text}"
    );

    // Verify that it found files in subdirectories
    assert!(
        scan_output_text.contains("Movies/Action/action1.mkv"),
        "Should find files in Movies/Action subdirectory, got: {scan_output_text}"
    );

    assert!(
        scan_output_text.contains("TV Shows/Show1/Season 1/episode1.webm"),
        "Should find files in nested TV show subdirectory, got: {scan_output_text}"
    );

    // Verify job count - should create 5 jobs (2 webm with vtt + 3 mkv)
    assert!(
        scan_output_text.contains("Added 5 new jobs"),
        "Expected 5 jobs to be created, got: {scan_output_text}"
    );

    // Verify queue files were created
    let queue_dir = temp_path.join("_queue");
    assert!(queue_dir.exists());

    let job_count = std::fs::read_dir(&queue_dir)
        .unwrap()
        .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
        .count();

    assert_eq!(job_count, 5);

    // Clean up
    let clean_output = Command::new(PLEXIFY_BIN)
        .args([
            "clean",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute clean command");

    assert!(clean_output.status.success(), "Clean command failed");
}

/// Test that jobs created from different directories contain absolute paths
#[test]
fn test_absolute_paths_in_jobs() {
    let temp_dir = TempDir::new().unwrap();
    let media_path = temp_dir.path().join("media");
    let work_path = temp_dir.path().join("work");
    let scan_from_path = temp_dir.path().join("scan_from");

    // Create directory structure
    fs::create_dir_all(&media_path).unwrap();
    fs::create_dir_all(&work_path).unwrap();
    fs::create_dir_all(&scan_from_path).unwrap();

    // Create test media files
    fs::create_dir_all(media_path.join("Season_01")).unwrap();
    fs::write(
        media_path.join("Season_01/episode1.mkv"),
        "dummy mkv content",
    )
    .unwrap();
    fs::write(
        media_path.join("Season_01/episode2.webm"),
        "dummy webm content",
    )
    .unwrap();
    fs::write(
        media_path.join("Season_01/episode2.vtt"),
        "dummy subtitle content",
    )
    .unwrap();

    // Change to a different directory before scanning. PLEXIFY_BIN is absolute,
    // so it stays valid across the change.
    let original_dir = std::env::current_dir().unwrap();
    std::env::set_current_dir(&scan_from_path).unwrap();

    // Run scan command from the different directory
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            media_path.to_str().unwrap(),
            "--work-dir",
            work_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    // Restore original directory
    std::env::set_current_dir(original_dir).unwrap();

    assert!(scan_output.status.success(), "Scan command failed");

    // Verify jobs were created
    let queue_dir = work_path.join("_queue");
    let job_files: Vec<_> = fs::read_dir(&queue_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "job" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    assert_eq!(job_files.len(), 2, "Should have created 2 job files");

    // Check that all jobs contain absolute paths
    for job_file in job_files {
        let job_content = fs::read_to_string(&job_file).unwrap();
        let job_json: serde_json::Value = serde_json::from_str(&job_content).unwrap();

        let input_path = job_json.get("input_path").unwrap().as_str().unwrap();
        let output_path = job_json.get("output_path").unwrap().as_str().unwrap();

        // Verify that paths are absolute
        assert!(
            Path::new(input_path).is_absolute(),
            "Input path should be absolute: {}",
            input_path
        );
        assert!(
            Path::new(output_path).is_absolute(),
            "Output path should be absolute: {}",
            output_path
        );

        // Verify paths point to the correct media directory
        assert!(
            input_path.starts_with(media_path.to_str().unwrap()),
            "Input path should start with media directory: {}",
            input_path
        );
        assert!(
            output_path.starts_with(media_path.to_str().unwrap()),
            "Output path should start with media directory: {}",
            output_path
        );

        // Check WebM subtitle paths are also absolute if present
        if let Some(subtitle_path) = job_json.get("subtitle_path") {
            if !subtitle_path.is_null() {
                let subtitle_path_str = subtitle_path.as_str().unwrap();
                assert!(
                    Path::new(subtitle_path_str).is_absolute(),
                    "Subtitle path should be absolute: {}",
                    subtitle_path_str
                );
                assert!(
                    subtitle_path_str.starts_with(media_path.to_str().unwrap()),
                    "Subtitle path should start with media directory: {}",
                    subtitle_path_str
                );
            }
        }
    }
}

/// Test that .plexifyignore files work in integration
#[test]
#[serial]
fn test_plexifyignore_integration() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create .plexifyignore file
    fs::write(temp_path.join(".plexifyignore"), "Downloads/\n*.tmp\ntools").unwrap();

    // Create directory structure
    fs::create_dir_all(temp_path.join("Downloads")).unwrap();
    fs::create_dir_all(temp_path.join("tools")).unwrap();
    fs::create_dir_all(temp_path.join("Anime")).unwrap();

    // Create media files - some should be ignored
    fs::write(temp_path.join("Downloads/video1.mkv"), "").unwrap();
    fs::write(temp_path.join("tools/video2.mkv"), "").unwrap();
    fs::write(temp_path.join("temp.tmp"), "").unwrap();
    fs::write(temp_path.join("Anime/episode1.mkv"), "").unwrap();
    fs::write(temp_path.join("movie.mkv"), "").unwrap();

    // Test scan command with debug logging to see ignore messages
    let scan_output = Command::new(PLEXIFY_BIN)
        .env("RUST_LOG", "info")
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    let scan_output_text = String::from_utf8_lossy(&scan_output.stderr);
    let scan_stdout_text = String::from_utf8_lossy(&scan_output.stdout);
    println!("Scan stderr: {}", scan_output_text);
    println!("Scan stdout: {}", scan_stdout_text);

    // Check that ignored message appears in either stdout or stderr, or that the correct file count is present
    let all_output = format!("{}{}", scan_output_text, scan_stdout_text);
    assert!(
        all_output.contains("Ignored") && all_output.contains("patterns")
            || all_output.contains("2 .mkv files"),
        "Expected ignore message or correct file count in output: stderr='{}' stdout='{}'",
        scan_output_text,
        scan_stdout_text
    );

    // Verify only non-ignored files were processed
    let queue_dir = temp_path.join("_queue");
    let job_files: Vec<_> = fs::read_dir(&queue_dir)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension()? == "job" {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Should only create jobs for Anime/episode1.mkv and movie.mkv (2 jobs)
    assert_eq!(job_files.len(), 2, "Expected 2 job files to be created");

    // Test validate command with debug logging
    let validate_output = Command::new(PLEXIFY_BIN)
        .env("RUST_LOG", "info")
        .args(["validate", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute validate command");

    assert!(validate_output.status.success(), "Validate command failed");

    let validate_output_text = String::from_utf8_lossy(&validate_output.stderr);
    let validate_stdout_text = String::from_utf8_lossy(&validate_output.stdout);
    println!("Validate stderr: {}", validate_output_text);
    println!("Validate stdout: {}", validate_stdout_text);

    // Should only validate 2 files (non-ignored ones)
    let all_validate_output = format!("{}{}", validate_output_text, validate_stdout_text);
    assert!(
        all_validate_output.contains("2 media files")
            || all_validate_output.contains("Ignored") && all_validate_output.contains("patterns"),
        "Expected correct file count or ignore message in validate output: stderr='{}' stdout='{}'",
        validate_output_text,
        validate_stdout_text
    );
}

/// Episodes are claimed in library order, from paths a real scan produced.
///
/// The queue is filled by running `scan` over a directory tree rather than by
/// building jobs in memory, because that walk is the only thing that produces
/// the separators and the season directories a worker actually meets.
#[tokio::test]
#[serial]
async fn test_episode_prioritization_integration() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Two padded seasons, one unpadded, one `Specials`, and a long-running
    // series numbered past ninety-nine: the shapes a library holds before
    // anything has normalised it.
    let episodes = [
        "Series/Better Call Saul/Season 01/Better Call Saul S01E02 Mijo.mkv",
        "Series/Better Call Saul/Season 01/Better Call Saul S01E01 Uno.mkv",
        "Series/Breaking Bad/Season 1/Breaking Bad S01E03 Gray Matter.mkv",
        "Series/Breaking Bad/Season 1/Breaking Bad S01E01 Pilot.mkv",
        "Series/Firefly/Specials/Firefly S00E01 Here's How It Was.mkv",
        "Anime/One Piece/Season 01/One Piece S01E108 Dashing Onto The Scene.mkv",
        "Anime/One Piece/Season 01/One Piece S01E99 Spirit Of The Fight.mkv",
        "Movies/Action/The Matrix (1999).mkv",
    ];

    for episode in episodes {
        let path = episode
            .split('/')
            .fold(temp_path.to_path_buf(), |path, part| path.join(part));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "dummy content").unwrap();
    }

    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run scan command");

    assert!(
        scan_output.status.success(),
        "Scan command failed: {}",
        String::from_utf8_lossy(&scan_output.stderr)
    );

    let queue = JobQueue::new(temp_path.to_path_buf(), temp_path.to_path_buf());
    let mut claimed_order = Vec::new();
    while let Some(claimed) = queue.claim_job(Some(JobPriority::Episode)).await.unwrap() {
        claimed_order.push(
            claimed
                .job
                .input_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string(),
        );
        claimed.complete().await.unwrap();
    }

    let expected = [
        "Better Call Saul S01E01 Uno.mkv",
        "Better Call Saul S01E02 Mijo.mkv",
        "Breaking Bad S01E01 Pilot.mkv",
        "Breaking Bad S01E03 Gray Matter.mkv",
        "Firefly S00E01 Here's How It Was.mkv",
        "One Piece S01E99 Spirit Of The Fight.mkv",
        "One Piece S01E108 Dashing Onto The Scene.mkv",
        // Nothing parses as an episode here, so the film is claimed last.
        "The Matrix (1999).mkv",
    ];

    assert_eq!(
        claimed_order, expected,
        "jobs were not claimed in series, season and episode order"
    );
}

/// Test that work command accepts priority parameter but defaults to none
#[test]
#[serial]
fn test_work_priority_defaults() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create a simple media file
    fs::write(temp_path.join("movie.mkv"), "dummy content").unwrap();

    // Scan to create a job
    let scan_output = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to run scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    // Test help to ensure priority parameter is documented
    let help_output = Command::new(PLEXIFY_BIN)
        .args(["work", "--help"])
        .output()
        .expect("Failed to run help command");

    assert!(help_output.status.success(), "Help command failed");
    let help_text = String::from_utf8_lossy(&help_output.stdout);

    // Verify the priority option is documented with the correct default
    assert!(
        help_text.contains("--priority"),
        "Help should document priority option"
    );
    assert!(
        help_text.contains("none"),
        "Help should show 'none' as an option"
    );
    assert!(
        help_text.contains("episode"),
        "Help should show 'episode' as an option"
    );
}

#[test]
fn test_rescanning_a_library_does_not_duplicate_jobs() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    fs::write(temp_path.join("video1.mkv"), "").unwrap();
    fs::write(temp_path.join("video2.mkv"), "").unwrap();

    let count_jobs = || {
        std::fs::read_dir(temp_path.join("_queue"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()? == "job").then_some(path)
            })
            .count()
    };

    let scan = || {
        let output = Command::new(PLEXIFY_BIN)
            .args([
                "scan",
                temp_path.to_str().unwrap(),
                "--work-dir",
                temp_path.to_str().unwrap(),
            ])
            .output()
            .expect("Failed to execute scan command");
        assert!(output.status.success(), "Scan command failed");
    };

    scan();
    assert_eq!(count_jobs(), 2, "first scan should queue both files");

    scan();
    assert_eq!(
        count_jobs(),
        2,
        "second scan should recognise both jobs as already queued"
    );
}

#[test]
fn test_validate_reports_without_changing_anything() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let season = temp_path.join("Series/Elementary/Season 6");
    fs::create_dir_all(&season).unwrap();
    let original = season.join("Elementary - S06E08 Sand Trap.mkv");
    fs::write(&original, "").unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args(["validate", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute validate command");

    assert!(output.status.success(), "Validate command failed");

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"),
        "the report should name the destination, got: {text}"
    );
    assert!(
        original.exists(),
        "a report without --fix must leave the library alone"
    );
}

#[test]
fn test_validate_fix_moves_files_to_their_canonical_paths() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    let plans = TempDir::new().unwrap();

    let season = temp_path.join("Series/Elementary/Season 6");
    fs::create_dir_all(&season).unwrap();
    fs::write(season.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();
    fs::write(season.join("Elementary - S06E08 Sand Trap.en.srt"), "").unwrap();

    // A path nothing can be proposed for, which must survive untouched.
    let nested = temp_path.join("Series/Veronica Mars/Series/Season 01");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Veronica Mars S02E04.mp4"), "").unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args(["validate", temp_path.to_str().unwrap(), "--fix"])
        .current_dir(plans.path())
        .output()
        .expect("Failed to execute validate --fix command");

    assert!(output.status.success(), "Validate --fix command failed");

    let fixed = temp_path.join("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv");
    assert!(
        fixed.exists(),
        "the episode should be at its canonical path"
    );
    assert!(
        temp_path
            .join("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.en.srt")
            .exists(),
        "the subtitle should have followed its episode"
    );
    assert!(
        nested.join("Veronica Mars S02E04.mp4").exists(),
        "a path needing a decision must not be moved"
    );

    // The run records what it did, next to where it was run from.
    let plan_files: Vec<_> = std::fs::read_dir(plans.path())
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().to_string();
            name.starts_with("plexify-fix-").then_some(name)
        })
        .collect();
    assert_eq!(
        plan_files.len(),
        1,
        "expected one plan file: {plan_files:?}"
    );

    // Running again finds nothing left to rename.
    let second = Command::new(PLEXIFY_BIN)
        .args(["validate", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute validate command");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(
        text.contains("Renames proposed: 0"),
        "the fix should be idempotent, got: {text}"
    );
}

#[test]
fn test_validate_fix_scoped_to_one_series_leaves_the_rest_alone() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    let plans = TempDir::new().unwrap();

    let target = temp_path.join("Series/Elementary/Season 6");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();

    let untouched = temp_path.join("Series/Scrubs/Season 9");
    fs::create_dir_all(&untouched).unwrap();
    let scrubs = untouched.join("Scrubs.S09E02.RETAIL.DVDRip.XviD-REWARD.avi");
    fs::write(&scrubs, "").unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args([
            "validate",
            temp_path.join("Series/Elementary").to_str().unwrap(),
            "--fix",
        ])
        .current_dir(plans.path())
        .output()
        .expect("Failed to execute scoped validate --fix");

    assert!(output.status.success(), "Scoped validate --fix failed");

    assert!(
        temp_path
            .join("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv")
            .exists(),
        "the scoped series should have been fixed"
    );
    assert!(
        scrubs.exists(),
        "a series outside the scope must not be touched"
    );

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("Library root:"),
        "a scoped run should say what it is judging against: {text}"
    );
}

#[test]
fn test_validate_accepts_a_relative_path_from_inside_the_library() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let season = temp_path.join("Series/Elementary/Season 6");
    fs::create_dir_all(&season).unwrap();
    fs::write(season.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();

    // Run from inside the library, naming the series relatively.
    let output = Command::new(PLEXIFY_BIN)
        .args(["validate", "Series/Elementary"])
        .current_dir(temp_path)
        .output()
        .expect("Failed to execute validate");

    assert!(
        output.status.success(),
        "validate failed on a relative path"
    );

    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"),
        "a relative path should resolve to the same destination as an absolute one: {text}"
    );
}

#[test]
fn test_undo_puts_a_fix_run_back() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();
    let plans = TempDir::new().unwrap();

    let season = temp_path.join("Series/Elementary/Season 6");
    fs::create_dir_all(&season).unwrap();
    let original = season.join("Elementary - S06E08 Sand Trap.mkv");
    fs::write(&original, "video").unwrap();
    let original_subtitle = season.join("Elementary - S06E08 Sand Trap.en.srt");
    fs::write(&original_subtitle, "subs").unwrap();

    let fix = Command::new(PLEXIFY_BIN)
        .args(["validate", temp_path.to_str().unwrap(), "--fix"])
        .current_dir(plans.path())
        .output()
        .expect("Failed to execute validate --fix");
    assert!(fix.status.success(), "validate --fix failed");
    assert!(!original.exists(), "the fix should have moved the episode");

    let plan_file = std::fs::read_dir(plans.path())
        .unwrap()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().to_string();
            name.starts_with("plexify-fix-").then_some(name)
        })
        .next()
        .expect("the fix should have written a plan");

    // A dry run says what it would do and touches nothing.
    let dry_run = Command::new(PLEXIFY_BIN)
        .args(["undo", &plan_file])
        .current_dir(plans.path())
        .output()
        .expect("Failed to execute undo");
    assert!(dry_run.status.success(), "undo dry run failed");
    assert!(
        !original.exists(),
        "a dry run must not put anything back yet"
    );

    let applied = Command::new(PLEXIFY_BIN)
        .args(["undo", &plan_file, "--apply"])
        .current_dir(plans.path())
        .output()
        .expect("Failed to execute undo --apply");
    assert!(applied.status.success(), "undo --apply failed");

    assert!(original.exists(), "the episode should be back where it was");
    assert!(
        original_subtitle.exists(),
        "the subtitle should have come back with it"
    );

    let undo_records = std::fs::read_dir(plans.path())
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .map(|e| e.file_name().to_string_lossy().starts_with("plexify-undo-"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        undo_records, 1,
        "an undo records what it did, so it can be undone in turn"
    );
}

#[test]
fn test_undo_rejects_a_file_that_is_not_a_plan() {
    let temp_dir = TempDir::new().unwrap();
    let not_a_plan = temp_dir.path().join("notes.json");
    fs::write(&not_a_plan, "{\"unrelated\": true}").unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args(["undo", not_a_plan.to_str().unwrap()])
        .output()
        .expect("Failed to execute undo");

    assert!(
        !output.status.success(),
        "undo should refuse a file it did not write"
    );
}

/// `status` answers for a work root nothing has ever scanned into, and does not
/// create one by being asked.
///
/// This is the shape of the `-w` mistake: `scan` ran in one shell and `work` in
/// another, so the queue the user is asking about is empty because it is not the
/// queue they made. Erroring here - or quietly initialising the directories and
/// reporting four zeroes with no explanation - both fail the user at the one
/// moment the answer is worth anything.
#[test]
fn test_status_explains_a_work_root_that_was_never_scanned_into() {
    let temp_dir = TempDir::new().unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args(["status", "--work-dir", temp_dir.path().to_str().unwrap()])
        .output()
        .expect("Failed to execute status command");

    assert!(
        output.status.success(),
        "status must answer for an uninitialised work root, not fail on it"
    );

    let text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(text.contains("never held a job"), "got: {text}");
    assert!(text.contains("-w/--work-dir"), "got: {text}");

    assert!(
        !temp_dir.path().join("_queue").exists(),
        "asking about a queue must not create one"
    );
}

/// A work root a scan initialised and put nothing in is an empty queue, not the
/// `-w` mistake above.
///
/// Scanning a directory with no media in it is the ordinary way to reach this:
/// the queue directories get created, no job is enqueued, and the flag names
/// exactly the right place. Printing the `-w` advice here sends the user to
/// change the one thing that is correct, which is the same failure the
/// unreachable work root has.
#[test]
#[serial]
fn test_status_tells_an_empty_queue_apart_from_a_misaddressed_one() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    let scan = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");
    assert!(scan.status.success());
    assert!(temp_path.join("_queue").is_dir(), "scan must initialise");

    let output = Command::new(PLEXIFY_BIN)
        .args(["status", "-w", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute status command");

    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(text.contains("This queue is empty"), "got: {text}");
    assert!(!text.contains("never held a job"), "got: {text}");
    assert!(!text.contains("-w/--work-dir"), "got: {text}");
}

/// A work root that cannot be listed must fail loudly rather than report an
/// empty queue, because "empty" here would be a claim about a place we could not
/// look - and the advice attached to it points at a flag that is not the problem.
#[test]
fn test_status_fails_on_a_work_root_it_cannot_read() {
    let temp_dir = TempDir::new().unwrap();

    // A file where `_queue` should be: a path that exists and cannot be listed,
    // which is what an unreachable shared work root looks like from here.
    fs::write(temp_dir.path().join("_queue"), "not a directory").unwrap();

    let output = Command::new(PLEXIFY_BIN)
        .args(["status", "-w", temp_dir.path().to_str().unwrap()])
        .output()
        .expect("Failed to execute status command");

    assert!(
        !output.status.success(),
        "a work root that could not be read must not exit 0"
    );

    // Both streams: the failure is logged through `tracing`, whose fmt layer
    // writes to stdout, so which one carries it is not this test's business.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("could not be reached"), "got: {text}");
    assert!(!text.contains("never held a job"), "got: {text}");
}

/// `status` counts a real queue and names the work root it counted.
///
/// The work root is asserted because `-w` defaults to the current working
/// directory: a report that does not say which queue it read is unusable for the
/// one mistake it most needs to catch.
#[test]
#[serial]
fn test_status_reports_a_scanned_queue_without_changing_it() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    fs::write(temp_path.join("one.mkv"), "").unwrap();
    fs::write(temp_path.join("two.mkv"), "").unwrap();

    let scan = Command::new(PLEXIFY_BIN)
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--work-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");
    assert!(scan.status.success());

    let mut before: Vec<_> = fs::read_dir(temp_path.join("_queue"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    before.sort();
    assert_eq!(before.len(), 2);

    let output = Command::new(PLEXIFY_BIN)
        .args(["status", "-w", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute status command");

    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(text.contains("Queued:      2"), "got: {text}");
    assert!(text.contains("Work root:"), "got: {text}");
    assert!(!text.contains("never held a job"), "got: {text}");

    // Read-only is the whole contract. Asking must leave the queue exactly as it
    // was, or `status` is not safe to point at a work root a worker is using.
    let mut after: Vec<_> = fs::read_dir(temp_path.join("_queue"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    after.sort();
    assert_eq!(before, after, "status must not move or rewrite anything");
}
