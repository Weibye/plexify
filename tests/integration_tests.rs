use std::process::Command;
use tempfile::TempDir;
use std::fs;

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

    assert!(build_output.status.success(), "Failed to build plexify binary");

    // Test scan command
    let scan_output = Command::new("./target/debug/plexify")
        .args(["scan", temp_path.to_str().unwrap()])
        .output()
        .expect("Failed to execute scan command");

    assert!(scan_output.status.success(), "Scan command failed");
    
    let scan_stdout = String::from_utf8_lossy(&scan_output.stdout);
    let scan_stderr = String::from_utf8_lossy(&scan_output.stderr);
    let scan_output_text = format!("{}{}", scan_stdout, scan_stderr);
    
    assert!(scan_output_text.contains("Added 2 new jobs"), "Expected 2 jobs to be created, got: {}", scan_output_text);
    assert!(scan_output_text.contains("SKIPPING: Missing subtitle file"), "Expected video3.webm to be skipped, got: {}", scan_output_text);

    // Verify queue files were created
    assert!(temp_path.join("_queue").exists());
    assert!(temp_path.join("_queue/video1.job").exists());
    assert!(temp_path.join("_queue/video2.job").exists());
    assert!(!temp_path.join("_queue/video3.job").exists());

    // Test clean command
    let clean_output = Command::new("./target/debug/plexify")
        .args(["clean", temp_path.to_str().unwrap()])
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
    assert!(help_stdout.contains("plexify"), "Help should contain program name");
    assert!(help_stdout.contains("scan"), "Help should list scan command");
    assert!(help_stdout.contains("work"), "Help should list work command");
    assert!(help_stdout.contains("clean"), "Help should list clean command");
}

/// Test that invalid paths are handled gracefully
#[test]
fn test_invalid_paths() {
    // Test scan with non-existent directory
    let scan_output = Command::new("./target/debug/plexify")
        .args(["scan", "/non/existent/path"])
        .output()
        .expect("Failed to execute scan command");

    assert!(!scan_output.status.success(), "Scan should fail with invalid path");

    // Test work with non-existent directory
    let work_output = Command::new("./target/debug/plexify")
        .args(["work", "/non/existent/path"])
        .output()
        .expect("Failed to execute work command");

    assert!(!work_output.status.success(), "Work should fail with invalid path");

    // Test clean with non-existent directory
    let clean_output = Command::new("./target/debug/plexify")
        .args(["clean", "/non/existent/path"])
        .output()
        .expect("Failed to execute clean command");

    assert!(!clean_output.status.success(), "Clean should fail with invalid path");
}