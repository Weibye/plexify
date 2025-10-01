use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::ignore::IgnoreFilter;
use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
use crate::queue::JobQueue;

/// Command to scan a directory for media files and create jobs
pub struct ScanCommand {
    media_root: PathBuf,
    work_root: PathBuf,
    preset: Option<String>,
}

impl ScanCommand {
    pub fn new(media_root: PathBuf, work_root: PathBuf, preset: Option<String>) -> Self {
        Self {
            media_root,
            work_root,
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

        // Initialize ignore filter
        let ignore_filter = match IgnoreFilter::new(self.media_root.clone()) {
            Ok(filter) => Some(filter),
            Err(e) => {
                warn!("Failed to load .plexifyignore patterns: {}", e);
                None
            }
        };

        let queue = JobQueue::new(self.media_root.clone(), self.work_root.clone());
        queue.init().await?;

        let mut webm_files = Vec::new();
        let mut mkv_files = Vec::new();
        let mut directories_scanned = std::collections::HashSet::new();
        let mut ignored_count = 0;

        // Walk through the directory to find media files
        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let path = e.path();

                // Always allow the root directory
                if path == self.media_root {
                    return true;
                }

                // Check if we should skip this directory and all its contents
                if path.is_dir() {
                    if let Some(ref filter) = ignore_filter {
                        if filter.should_skip_dir(path) {
                            debug!("🚫 Skipping entire directory: {:?}", path);
                            return false; // This will cause WalkDir to skip the directory
                        }
                    }
                }

                true // Allow files and non-ignored directories
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Check if this individual path should be ignored
            if let Some(ref filter) = ignore_filter {
                if filter.should_ignore(path) {
                    debug!("🚫 Ignoring path: {:?}", path);
                    ignored_count += 1;
                    // Skip this entry completely
                    continue;
                }
            }

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

        if ignored_count > 0 {
            info!(
                "📋 Ignored {} paths due to .plexifyignore patterns",
                ignored_count
            );
        }

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
                &self.media_root,
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
                MediaFileType::Mkv,
                quality_settings.clone(),
                post_processing.clone(),
                &self.media_root,
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
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
        );

        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_nonexistent_directory() {
        let scan_cmd = ScanCommand::new(
            PathBuf::from("/nonexistent/path"),
            PathBuf::from("/tmp"),
            None,
        );

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

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
        );
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

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
        );
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

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
        );
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

    #[tokio::test]
    async fn test_scan_with_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\n*.tmp\ntools",
        )
        .unwrap();

        // Create directory structure
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Anime")).unwrap();

        // Create media files - some should be ignored
        fs::write(media_root.join("Downloads/video1.mkv"), "").unwrap();
        fs::write(media_root.join("tools/video2.mkv"), "").unwrap();
        fs::write(media_root.join("temp.tmp"), "").unwrap();
        fs::write(media_root.join("Anime/episode1.mkv"), "").unwrap();
        fs::write(media_root.join("movie.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
        );
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Check job files - should only create jobs for non-ignored files
        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should only create jobs for Anime/episode1.mkv and movie.mkv (2 jobs)
        assert_eq!(job_count, 2);
    }

    #[tokio::test]
    async fn test_scan_with_nested_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create root .plexifyignore
        fs::write(media_root.join(".plexifyignore"), "*.tmp").unwrap();

        // Create nested directory with its own .plexifyignore
        fs::create_dir_all(media_root.join("Series")).unwrap();
        fs::write(
            media_root.join("Series/.plexifyignore"),
            "old/\n!important.mkv",
        )
        .unwrap();

        // Create test files
        fs::create_dir_all(media_root.join("Series/old")).unwrap();
        fs::write(media_root.join("test.tmp"), "").unwrap();
        fs::write(media_root.join("Series/show.mkv"), "").unwrap();
        fs::write(media_root.join("Series/old/episode.mkv"), "").unwrap();
        fs::write(media_root.join("Series/important.mkv"), "").unwrap();
        fs::write(media_root.join("movie.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
        );
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Check job files
        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should create jobs for: Series/show.mkv, Series/important.mkv, movie.mkv (3 jobs)
        // Should ignore: test.tmp (root pattern), Series/old/episode.mkv (nested pattern)
        assert_eq!(job_count, 3);
    }
}
