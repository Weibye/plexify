use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
use crate::queue::JobQueue;

/// Command to scan a directory for media files and create jobs
pub struct ScanCommand {
    media_root: PathBuf,
    queue_root: PathBuf,
    preset: Option<String>,
}

impl ScanCommand {
    pub fn new(media_root: PathBuf, queue_root: PathBuf, preset: Option<String>) -> Self {
        Self {
            media_root,
            queue_root,
            preset,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.media_root.exists() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.media_root
            ));
        }

        if !self.media_root.is_dir() {
            return Err(anyhow!("Path is not a directory: {:?}", self.media_root));
        }

        info!("🔎 Scanning directory: {:?}", self.media_root);
        info!("📁 Recursively scanning all subdirectories...");

        let queue = JobQueue::new(self.media_root.clone(), self.queue_root.clone());
        queue.init().await?;

        let mut webm_files = Vec::new();
        let mut mkv_files = Vec::new();
        let mut directories_scanned = std::collections::HashSet::new();

        // Walk through the directory to find media files
        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Track directories being scanned for better user feedback
            if path.is_dir() && path != self.media_root {
                if let Ok(relative_dir) = path.strip_prefix(&self.media_root) {
                    if !directories_scanned.contains(relative_dir) {
                        directories_scanned.insert(relative_dir.to_path_buf());
                        debug!("📂 Scanning subdirectory: {:?}", relative_dir);
                    }
                }
            }

            if path.is_file() {
                if let Some(extension) = path.extension() {
                    let ext_str = extension.to_string_lossy().to_lowercase();
                    match ext_str.as_str() {
                        "webm" => {
                            if let Ok(relative_path) = path.strip_prefix(&self.media_root) {
                                webm_files.push(relative_path.to_path_buf());
                            }
                        }
                        "mkv" => {
                            if let Ok(relative_path) = path.strip_prefix(&self.media_root) {
                                mkv_files.push(relative_path.to_path_buf());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        info!(
            "📊 Scanned {} directories and found {} .webm files and {} .mkv files",
            directories_scanned.len(),
            webm_files.len(),
            mkv_files.len()
        );

        if !directories_scanned.is_empty() {
            debug!(
                "📋 Scanned subdirectories: {:?}",
                directories_scanned.iter().collect::<Vec<_>>()
            );
        }

        info!("🔄 Now creating transcoding jobs...");

        let mut job_count = 0;

        // Get configuration settings for jobs
        let quality_settings = match &self.preset {
            Some(preset_name) => {
                info!("Using quality preset: '{}'", preset_name);
                QualitySettings::from_preset_name(preset_name)?
            }
            None => {
                info!("Using quality settings from environment variables");
                QualitySettings::from_env()
            }
        };
        let post_processing = PostProcessingSettings::default();

        // Process WebM files (require VTT subtitles)
        for webm_path in webm_files {
            let job = Job::new(
                webm_path.clone(),
                MediaFileType::WebM,
                quality_settings.clone(),
                post_processing.clone(),
            );

            // Check if output already exists
            if job.output_exists(Some(&self.media_root)) {
                debug!("Output already exists for: {:?}", webm_path);
                continue;
            }

            // Check if job already exists in queue
            if queue.job_exists(&job).await? {
                debug!("Job already exists for: {:?}", webm_path);
                continue;
            }

            // Check if required subtitle file exists
            if !job.has_required_subtitle(Some(&self.media_root))? {
                warn!("⚠️ SKIPPING: Missing subtitle file for '{:?}'", webm_path);
                continue;
            }

            // Create the job
            queue.enqueue_job(&job).await?;
            info!("➕ Queueing job for: {:?}", webm_path);
            job_count += 1;
        }

        // Process MKV files (embedded subtitles)
        for mkv_path in mkv_files {
            let job = Job::new(
                mkv_path.clone(),
                MediaFileType::MKV,
                quality_settings.clone(),
                post_processing.clone(),
            );

            // Check if output already exists
            if job.output_exists(Some(&self.media_root)) {
                debug!("Output already exists for: {:?}", mkv_path);
                continue;
            }

            // Check if job already exists in queue
            if queue.job_exists(&job).await? {
                debug!("Job already exists for: {:?}", mkv_path);
                continue;
            }

            // Create the job
            queue.enqueue_job(&job).await?;
            info!(
                "➕ Queueing job for: {:?} (embedded subs assumed)",
                mkv_path
            );
            job_count += 1;
        }

        info!(
            "✅ Scan complete. Added {} new jobs to the queue.",
            job_count
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd =
            ScanCommand::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf(), None);

        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_nonexistent_directory() {
        let scan_cmd = ScanCommand::new(PathBuf::from("/nonexistent/path"), PathBuf::from("/tmp"), None);

        let result = scan_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_scan_with_preset() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
        );

        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_with_invalid_preset() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("invalid_preset".to_string()),
        );

        let result = scan_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_scan_hierarchical_directories() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create hierarchical directory structure
        fs::create_dir_all(media_root.join("show1/season1")).unwrap();
        fs::create_dir_all(media_root.join("show2/season2")).unwrap();
        fs::create_dir_all(media_root.join("movies")).unwrap();
        fs::create_dir_all(media_root.join("very/deep/nested/folder")).unwrap();

        // Create media files in different subdirectories
        fs::write(media_root.join("show1/season1/episode1.webm"), "").unwrap();
        fs::write(media_root.join("show1/season1/episode1.vtt"), "").unwrap();
        fs::write(media_root.join("show2/season2/episode2.mkv"), "").unwrap();
        fs::write(media_root.join("movies/movie1.mkv"), "").unwrap();
        fs::write(media_root.join("very/deep/nested/folder/deep.webm"), "").unwrap();
        fs::write(media_root.join("very/deep/nested/folder/deep.vtt"), "").unwrap();

        let scan_cmd = ScanCommand::new(media_root.to_path_buf(), temp_dir.path().to_path_buf());
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Verify queue directory was created and contains job files
        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Count job files - should have created jobs for all media files with proper subtitles
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

        // Should have 4 jobs: 2 webm files with subtitles + 2 mkv files
        assert_eq!(job_files.len(), 4);
    }

    #[tokio::test]
    async fn test_scan_finds_nested_media_files() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create a deep nested structure
        let deep_path = media_root.join("level1/level2/level3/level4");
        fs::create_dir_all(&deep_path).unwrap();

        // Create media file at different depths
        fs::write(media_root.join("root.mkv"), "").unwrap();
        fs::write(media_root.join("level1/l1.mkv"), "").unwrap();
        fs::write(deep_path.join("deep.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(media_root.to_path_buf(), temp_dir.path().to_path_buf());
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Should find all 3 mkv files regardless of nesting depth
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        assert_eq!(job_count, 3);
    }

    #[tokio::test]
    async fn test_scan_mixed_media_types_in_hierarchy() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create different media types in different folders
        fs::create_dir_all(media_root.join("webm_folder")).unwrap();
        fs::create_dir_all(media_root.join("mkv_folder")).unwrap();
        fs::create_dir_all(media_root.join("mixed_folder")).unwrap();

        // WebM files (need matching VTT)
        fs::write(media_root.join("webm_folder/video1.webm"), "").unwrap();
        fs::write(media_root.join("webm_folder/video1.vtt"), "").unwrap();
        fs::write(media_root.join("webm_folder/video2.webm"), "").unwrap(); // No VTT - should be skipped

        // MKV files
        fs::write(media_root.join("mkv_folder/video1.mkv"), "").unwrap();
        fs::write(media_root.join("mkv_folder/video2.mkv"), "").unwrap();

        // Mixed folder
        fs::write(media_root.join("mixed_folder/mixed1.webm"), "").unwrap();
        fs::write(media_root.join("mixed_folder/mixed1.vtt"), "").unwrap();
        fs::write(media_root.join("mixed_folder/mixed2.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(media_root.to_path_buf(), temp_dir.path().to_path_buf());
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should create jobs for: 2 webm files with VTT + 3 mkv files = 5 jobs
        // (video2.webm without VTT should be skipped)
        assert_eq!(job_count, 5);
    }
}
