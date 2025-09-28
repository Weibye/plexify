use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal;
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::ffmpeg::FFmpegProcessor;
use crate::queue::JobQueue;

/// Command to process jobs from the queue
pub struct WorkCommand {
    media_root: PathBuf,
    background_mode: bool,
}

impl WorkCommand {
    pub fn new(media_root: PathBuf, background_mode: bool) -> Self {
        Self {
            media_root,
            background_mode,
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

        let config = Config::from_env();
        let mode = if self.background_mode {
            "Low Priority Worker"
        } else {
            "Power Worker (Foreground)"
        };

        info!("✅ Starting worker in {} mode.", mode);
        info!("Watching for jobs in: {:?}", self.media_root.join("_queue"));

        let queue = JobQueue::new(self.media_root.clone());
        queue.init().await?;

        let processor = FFmpegProcessor::new(config.clone(), self.background_mode);

        // Set up signal handling for graceful shutdown
        tokio::pin! {
            let shutdown_signal = signal::ctrl_c();
        }

        loop {
            tokio::select! {
                // Check for shutdown signal
                _ = &mut shutdown_signal => {
                    info!("🛑 Shutdown signal received. Exiting gracefully.");
                    break;
                }

                // Try to claim and process a job
                job_result = self.process_next_job(&queue, &processor) => {
                    match job_result {
                        Ok(true) => {
                            // Job was processed, continue immediately to check for more
                            continue;
                        }
                        Ok(false) => {
                            // No job available, sleep
                            debug!("💤 No jobs found. Sleeping for {} seconds.", config.sleep_interval);
                            tokio::time::sleep(Duration::from_secs(config.sleep_interval)).await;
                        }
                        Err(e) => {
                            error!("Error processing job: {}", e);
                            // Sleep a bit before retrying
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Try to claim and process the next job from the queue
    /// Returns Ok(true) if a job was processed, Ok(false) if no job was available
    async fn process_next_job(
        &self,
        queue: &JobQueue,
        processor: &FFmpegProcessor,
    ) -> Result<bool> {
        if let Some(claimed_job) = queue.claim_job().await? {
            info!("➡️ Claimed job: {}", claimed_job.job_name());

            // Get the job details directly from the job file
            let job = &claimed_job.job;

            // Process the job with FFmpeg using the job's own media_root (for absolute paths) or self.media_root (for relative paths)
            let media_root = if job.input_path.is_absolute() {
                None
            } else {
                Some(self.media_root.as_path())
            };

            let job_name = claimed_job.job_name().to_string();
            match processor.process_job(job, media_root).await {
                Ok(_) => {
                    // Conversion successful - disable source files if configured and mark job complete
                    if job.post_processing.disable_source_files {
                        if let Err(e) = processor.disable_source_files(job, media_root).await {
                            warn!("Failed to disable source files: {}", e);
                            // Continue anyway, the conversion was successful
                        }
                    }

                    claimed_job.complete().await?;
                    info!("✅ Job completed successfully: {}", job_name);
                }
                Err(e) => {
                    error!("❌ Conversion FAILED: {}", e);
                    claimed_job.return_to_queue().await?;

                    // Sleep a bit to avoid rapid retries of problematic jobs
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }

            Ok(true) // We processed a job
        } else {
            Ok(false) // No job available
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_work_command_creation() {
        let temp_dir = TempDir::new().unwrap();
        let work_cmd = WorkCommand::new(temp_dir.path().to_path_buf(), false);

        assert_eq!(work_cmd.media_root, temp_dir.path());
        assert!(!work_cmd.background_mode);
    }

    #[tokio::test]
    async fn test_work_nonexistent_directory() {
        let work_cmd = WorkCommand::new(PathBuf::from("/nonexistent/path"), false);

        let result = work_cmd.execute().await;
        assert!(result.is_err());
    }
}
