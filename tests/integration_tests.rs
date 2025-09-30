use std::fs;
use std::process::Command;
use tempfile::TempDir;

/// Test the complete scan -> clean workflow
#[test]
fn test_scan_and_clean_workflow() {
    let temp_dir = TempDir::new().unwrap();
    let temp_path = temp_dir.path();

    // Create test files
    fs::write(temp_path.join("video1.webm"), "").unwrap();
    fs::write(temp_path.join("video1.vtt"), "").unwrap();
    fs::write(temp_path.join("video2.mkv"), "").unwrap();
    fs::write(temp_path.join("video3.webm"), "").unwrap(); // No .vtt file

    // Build the binary first
    let build_output = Command::new("cargo")
        .args(["build", "--bin", "plexify"])
        .output()
        .expect("Failed to build plexify");

    assert!(
        build_output.status.success(),
        "Failed to build plexify binary"
    );

    // Test scan command (use temp_path as both media dir and queue dir)
    let scan_output = Command::new("./target/debug/plexify")
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
    let clean_output = Command::new("./target/debug/plexify")
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
fn test_help_commands() {
    let help_output = Command::new("./target/debug/plexify")
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
fn test_invalid_paths() {
    // Test scan with non-existent directory
    let scan_output = Command::new("./target/debug/plexify")
        .args(["scan", "/non/existent/path"])
        .output()
        .expect("Failed to execute scan command");

    assert!(
        !scan_output.status.success(),
        "Scan should fail with invalid path"
    );

    // Test work with non-existent directory
    let work_output = Command::new("./target/debug/plexify")
        .args(["work", "/non/existent/path"])
        .output()
        .expect("Failed to execute work command");

    assert!(
        !work_output.status.success(),
        "Work should fail with invalid path"
    );

    // Test clean with non-existent directory
    let clean_output = Command::new("./target/debug/plexify")
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
fn test_job_files_contain_complete_details() {
    // Build the binary first
    let build_output = Command::new("cargo")
        .args(["build", "--bin", "plexify"])
        .output()
        .expect("Failed to build plexify");

    assert!(
        build_output.status.success(),
        "Failed to build plexify binary"
    );

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
    let scan_output = Command::new("./target/debug/plexify")
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
fn test_work_folder_workflow() {
    // Build the binary first
    let build_output = Command::new("cargo")
        .args(["build", "--bin", "plexify"])
        .output()
        .expect("Failed to build plexify");

    assert!(
        build_output.status.success(),
        "Failed to build plexify binary"
    );

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
    let scan_output = Command::new("./target/debug/plexify")
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

/// Test hierarchical directory scanning functionality
#[test]
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
    let scan_output = Command::new("./target/debug/plexify")
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
    let clean_output = Command::new("./target/debug/plexify")
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
