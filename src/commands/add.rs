use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::{info, warn};

use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
use crate::queue::JobQueue;

/// Command to create a job for an individual media file
pub struct AddCommand {
    file_path: PathBuf,
    work_root: PathBuf,
    preset: Option<String>,
}

impl AddCommand {
    pub fn new(file_path: PathBuf, work_root: PathBuf, preset: Option<String>) -> Self {
        Self {
            file_path,
            work_root,
            preset,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.file_path.exists() {
            return Err(anyhow!("File does not exist: {:?}", self.file_path));
        }

        if !self.file_path.is_file() {
            return Err(anyhow!("Path is not a file: {:?}", self.file_path));
        }

        info!("📄 Processing file: {:?}", self.file_path);

        // Determine file type from extension
        let file_type = match self.file_path.extension() {
            Some(ext) => match ext.to_string_lossy().to_lowercase().as_str() {
                "webm" => MediaFileType::WebM,
                "mkv" => MediaFileType::Mkv,
                _ => {
                    return Err(anyhow!(
                        "Unsupported file type. Only .webm and .mkv files are supported. Got: {:?}",
                        self.file_path
                    ));
                }
            },
            None => {
                return Err(anyhow!(
                    "File has no extension. Only .webm and .mkv files are supported: {:?}",
                    self.file_path
                ));
            }
        };

        // Get the directory containing the file (this will be our media root)
        let media_root = self
            .file_path
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "Unable to determine parent directory for: {:?}",
                    self.file_path
                )
            })?
            .to_path_buf();

        let queue = JobQueue::new(media_root.clone(), self.work_root.clone());
        queue.init().await?;

        // Get configuration settings for the job
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

        // Get relative path from media root
        let relative_path = self
            .file_path
            .strip_prefix(&media_root)
            .map_err(|_| anyhow!("Unable to create relative path for: {:?}", self.file_path))?
            .to_path_buf();

        // Create the job
        let job = Job::new(
            relative_path.clone(),
            file_type.clone(),
            quality_settings,
            post_processing,
            &media_root,
        );

        // Check if output already exists
        if job.output_exists(Some(&media_root)) {
            warn!("⚠️ Output file already exists for: {:?}", relative_path);
            info!("✅ No action needed - output file already exists.");
            return Ok(());
        }

        // Check if job already exists in queue
        if queue.job_exists(&job).await? {
            warn!("⚠️ Job already exists in queue for: {:?}", relative_path);
            info!("✅ No action needed - job already queued.");
            return Ok(());
        }

        // For WebM files, check if required subtitle file exists
        if file_type == MediaFileType::WebM && !job.has_required_subtitle(Some(&media_root))? {
            return Err(anyhow!(
                "Missing required subtitle file (.vtt) for WebM file: {:?}",
                self.file_path
            ));
        }

        // Create the job
        queue.enqueue_job(&job).await?;

        match file_type {
            MediaFileType::WebM => {
                info!(
                    "✅ Successfully created transcoding job for: {:?}",
                    relative_path
                );
            }
            MediaFileType::Mkv => {
                info!(
                    "✅ Successfully created transcoding job for: {:?} (embedded subs assumed)",
                    relative_path
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_add_webm_file_with_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test files
        let webm_file = media_path.join("video.webm");
        let vtt_file = media_path.join("video.vtt");
        fs::write(&webm_file, "").unwrap();
        fs::write(&vtt_file, "").unwrap();

        let add_cmd = AddCommand::new(webm_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_ok());

        // Verify queue directory was created
        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Verify job file was created
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();
        assert_eq!(job_count, 1);
    }

    #[tokio::test]
    async fn test_add_mkv_file() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test file
        let mkv_file = media_path.join("video.mkv");
        fs::write(&mkv_file, "").unwrap();

        let add_cmd = AddCommand::new(mkv_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_ok());

        // Verify queue directory was created
        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Verify job file was created
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();
        assert_eq!(job_count, 1);
    }

    #[tokio::test]
    async fn test_add_webm_file_without_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create only webm file (no .vtt)
        let webm_file = media_path.join("video.webm");
        fs::write(&webm_file, "").unwrap();

        let add_cmd = AddCommand::new(webm_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing required subtitle file"));
    }

    #[tokio::test]
    async fn test_add_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let nonexistent_file = temp_dir.path().join("nonexistent.mkv");

        let add_cmd = AddCommand::new(nonexistent_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("File does not exist"));
    }

    #[tokio::test]
    async fn test_add_unsupported_file_type() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create unsupported file type
        let mp4_file = media_path.join("video.mp4");
        fs::write(&mp4_file, "").unwrap();

        let add_cmd = AddCommand::new(mp4_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unsupported file type"));
    }

    #[tokio::test]
    async fn test_add_with_preset() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test file
        let mkv_file = media_path.join("video.mkv");
        fs::write(&mkv_file, "").unwrap();

        let add_cmd = AddCommand::new(
            mkv_file,
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
        );

        let result = add_cmd.execute().await;
        assert!(result.is_ok());

        // Verify job was created
        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();
        assert_eq!(job_count, 1);
    }

    #[tokio::test]
    async fn test_add_existing_output() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test files
        let mkv_file = media_path.join("video.mkv");
        let mp4_file = media_path.join("video.mp4"); // Output already exists
        fs::write(&mkv_file, "").unwrap();
        fs::write(&mp4_file, "").unwrap();

        let add_cmd = AddCommand::new(mkv_file, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_ok());

        // Verify no job was created since output already exists
        let queue_dir = temp_dir.path().join("_queue");
        if queue_dir.exists() {
            let job_count = fs::read_dir(&queue_dir)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
                .count();
            assert_eq!(job_count, 0);
        }
    }

    #[tokio::test]
    async fn test_add_directory_fails() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create a directory
        let dir_path = media_path.join("directory");
        fs::create_dir(&dir_path).unwrap();

        let add_cmd = AddCommand::new(dir_path, temp_dir.path().to_path_buf(), None);

        let result = add_cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Path is not a file"));
    }
}
