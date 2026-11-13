use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::info;

use crate::paths::to_forward_slashes;
use crate::queue::JobQueue;

use super::job_processor::{operation_for, JobProcessResult, JobProcessor, JobProcessorConfig};

/// Command to create a job for an individual media file
pub struct AddCommand {
    file_path: PathBuf,
    work_root: PathBuf,
    config: JobProcessorConfig,
}

impl AddCommand {
    pub fn new(file_path: PathBuf, work_root: PathBuf, config: JobProcessorConfig) -> Self {
        Self {
            file_path,
            work_root,
            config,
        }
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.file_path.exists() {
            return Err(anyhow!(
                "File does not exist: {}",
                to_forward_slashes(&self.file_path)
            ));
        }

        if !self.file_path.is_file() {
            return Err(anyhow!(
                "Path is not a file: {}",
                to_forward_slashes(&self.file_path)
            ));
        }

        info!(
            "📄 Processing file: {}",
            to_forward_slashes(&self.file_path)
        );

        let file_type =
            JobProcessor::determine_file_type(&self.file_path).map_err(|err| anyhow!(err))?;

        // The file's own directory is the media root, so the job's paths are
        // resolved against it.
        let media_root = self
            .file_path
            .parent()
            .ok_or_else(|| {
                anyhow!(
                    "Unable to determine parent directory for: {}",
                    to_forward_slashes(&self.file_path)
                )
            })?
            .to_path_buf();

        let queue = JobQueue::new(media_root.clone(), self.work_root.clone());
        queue.init().await?;

        let relative_path = self
            .file_path
            .strip_prefix(&media_root)
            .map_err(|_| {
                anyhow!(
                    "Unable to create relative path for: {}",
                    to_forward_slashes(&self.file_path)
                )
            })?
            .to_path_buf();

        let processor = JobProcessor::new(&queue, &self.config, &media_root);
        // `add` names one file and asks no client about it, so the extension
        // decides. `scan --target` is where a conformance check belongs.
        let operation = operation_for(&file_type);
        let result = processor
            .process_media_file(&relative_path, file_type, operation)
            .await?;

        if result == JobProcessResult::MissingSubtitle {
            return Err(anyhow!(
                "Missing required subtitle file (.vtt) for WebM file: {}",
                to_forward_slashes(&self.file_path)
            ));
        }
        processor.log_result(&relative_path, operation, &result);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::QualityPreset;
    use std::fs;
    use tempfile::TempDir;

    /// Every job `add` left in the queue under `work_root`.
    fn queued_jobs(work_root: &std::path::Path) -> Vec<crate::job::Job> {
        fs::read_dir(work_root.join("_queue"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension() == Some("job".as_ref()))
            .map(|path| serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn test_add_webm_file_with_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test files
        let webm_file = media_path.join("video.webm");
        let vtt_file = media_path.join("video.vtt");
        fs::write(&webm_file, "").unwrap();
        fs::write(&vtt_file, "").unwrap();

        let add_cmd = AddCommand::new(webm_file, temp_dir.path().to_path_buf(), Default::default());

        add_cmd.execute().await.unwrap();

        assert_eq!(
            queued_jobs(temp_dir.path()).len(),
            1,
            "video.webm is queued beside its video.vtt"
        );
    }

    #[tokio::test]
    async fn test_add_mkv_file() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create test file
        let mkv_file = media_path.join("video.mkv");
        fs::write(&mkv_file, "").unwrap();

        let add_cmd = AddCommand::new(mkv_file, temp_dir.path().to_path_buf(), Default::default());

        add_cmd.execute().await.unwrap();

        assert_eq!(queued_jobs(temp_dir.path()).len(), 1, "video.mkv is queued");
    }

    #[tokio::test]
    async fn test_add_webm_file_without_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create only webm file (no .vtt)
        let webm_file = media_path.join("video.webm");
        fs::write(&webm_file, "").unwrap();

        let add_cmd = AddCommand::new(webm_file, temp_dir.path().to_path_buf(), Default::default());

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

        let add_cmd = AddCommand::new(
            nonexistent_file,
            temp_dir.path().to_path_buf(),
            Default::default(),
        );

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

        let add_cmd = AddCommand::new(mp4_file, temp_dir.path().to_path_buf(), Default::default());

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
            JobProcessorConfig {
                quality_settings: QualityPreset::Quality.to_quality_settings(),
                ..Default::default()
            },
        );

        add_cmd.execute().await.unwrap();

        let jobs = queued_jobs(temp_dir.path());
        assert_eq!(jobs.len(), 1, "video.mkv is queued");
        assert_eq!(
            jobs[0].quality_settings,
            QualityPreset::Quality.to_quality_settings(),
            "the job for video.mkv carries the preset it was added with"
        );
    }

    #[tokio::test]
    async fn test_add_existing_output() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        let mkv_file = media_path.join("video.mkv");
        let mp4_file = media_path.join("video.mp4");
        fs::write(&mkv_file, "").unwrap();
        fs::write(&mp4_file, "").unwrap();

        let add_cmd = AddCommand::new(mkv_file, temp_dir.path().to_path_buf(), Default::default());

        add_cmd.execute().await.unwrap();

        assert!(
            queued_jobs(temp_dir.path()).is_empty(),
            "video.mkv is not queued, because video.mp4 already exists"
        );
    }

    #[tokio::test]
    async fn test_add_directory_fails() {
        let temp_dir = TempDir::new().unwrap();
        let media_path = temp_dir.path();

        // Create a directory
        let dir_path = media_path.join("directory");
        fs::create_dir(&dir_path).unwrap();

        let add_cmd = AddCommand::new(dir_path, temp_dir.path().to_path_buf(), Default::default());

        let result = add_cmd.execute().await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Path is not a file"));
    }
}
