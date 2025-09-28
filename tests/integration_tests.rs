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
            "--queue-dir",
            temp_path.to_str().unwrap(),
        ])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");

    let scan_stdout = String::from_utf8_lossy(&scan_output.stdout);
    let scan_stderr = String::from_utf8_lossy(&scan_output.stderr);
    let scan_output_text = format!("{}{}", scan_stdout, scan_stderr);

    assert!(
        scan_output_text.contains("Added 2 new jobs"),
        "Expected 2 jobs to be created, got: {}",
        scan_output_text
    );
    assert!(
        scan_output_text.contains("SKIPPING: Missing subtitle file"),
        "Expected video3.webm to be skipped, got: {}",
        scan_output_text
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
        "Expected video3.webm to be skipped, got: {}",
        scan_output_text
    );

    // Test clean command
    let clean_output = Command::new("./target/debug/plexify")
        .args([
            "clean",
            temp_path.to_str().unwrap(),
            "--queue-dir",
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
    std::env::set_var("FFMPEG_PRESET", "fast");
    std::env::set_var("FFMPEG_CRF", "20");
    std::env::set_var("FFMPEG_AUDIO_BITRATE", "192k");

    // Run scan command (use temp_path as both media dir and queue dir)
    let scan_output = Command::new("./target/debug/plexify")
        .args([
            "scan",
            temp_path.to_str().unwrap(),
            "--queue-dir",
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

    // Clean up environment variables
    std::env::remove_var("FFMPEG_PRESET");
    std::env::remove_var("FFMPEG_CRF");
    std::env::remove_var("FFMPEG_AUDIO_BITRATE");
}
