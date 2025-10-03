use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use std::time::Duration;
use tokio::signal;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::ffmpeg::FFmpegProcessor;
use crate::queue::JobQueue;
use crate::JobPriority;

/// Command to process jobs from the queue
pub struct WorkCommand {
    media_root: PathBuf,
    work_root: PathBuf,
    background_mode: bool,
    priority_mode: JobPriority,
}

impl WorkCommand {
    pub fn new(
        media_root: PathBuf,
        work_root: PathBuf,
        background_mode: bool,
        priority_mode: JobPriority,
    ) -> Self {
        Self {
            media_root,
            work_root,
            background_mode,
            priority_mode,
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
        info!("Watching for jobs in: {:?}", self.work_root.join("_queue"));

        let queue = JobQueue::new(self.media_root.clone(), self.work_root.clone());
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
                            // No job available, sleep with progress bar
                            let sleep_duration = config.sleep_interval;

                            if sleep_duration > 5 {
                                // Show progress bar for sleep intervals longer than 5 seconds
                                let pb = ProgressBar::new(sleep_duration);
                                pb.set_style(
                                    ProgressStyle::with_template(
                                        "💤 Waiting for jobs {bar:30.cyan/blue} {pos}/{len}s {msg}"
                                    ).unwrap()
                                    .progress_chars("█▉▊▋▌▍▎▏ ")
                                );
                                pb.set_message("Watching queue...");

                                for _i in 0..sleep_duration {
                                    tokio::time::sleep(Duration::from_secs(1)).await;
                                    pb.inc(1);
                                }

                                pb.finish_and_clear();
                            } else {
                                tokio::time::sleep(Duration::from_secs(sleep_duration)).await;
                            }
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
        let priority = if self.priority_mode == JobPriority::None {
            None
        } else {
            Some(self.priority_mode.clone())
        };

        if let Some(claimed_job) = queue.claim_job(priority).await? {
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
            let work_folder = &queue.in_progress_dir;

            // Create a progress bar for job processing
            let job_pb = ProgressBar::new_spinner();
            job_pb.set_style(
                ProgressStyle::with_template("{spinner:.green} {msg}")
                    .unwrap()
                    .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
            );
            job_pb.set_message(format!("Processing: {}", job_name));
            job_pb.enable_steady_tick(Duration::from_millis(120));

            match processor
                .process_job(job, media_root, Some(work_folder))
                .await
            {
                Ok(_) => {
                    job_pb.set_message("Moving output file...");

                    // Move file from work folder to media folder
                    if let Err(e) = processor
                        .move_to_destination(job, media_root, work_folder)
                        .await
                    {
                        error!("Failed to move file from work folder: {}", e);
                        job_pb.finish_and_clear();
                        claimed_job.return_to_queue().await?;
                        return Ok(true);
                    }

                    // Disable source files if configured
                    if job.post_processing.disable_source_files {
                        job_pb.set_message("Cleaning up source files...");
                        if let Err(e) = processor.disable_source_files(job, media_root).await {
                            warn!("Failed to disable source files: {}", e);
                            // Continue anyway, the conversion was successful
                        }
                    }

                    job_pb.finish_with_message(format!("✅ Completed: {}", job_name));
                    claimed_job.complete().await?;
                }
                Err(e) => {
                    job_pb.finish_with_message(format!("❌ Failed: {}", job_name));
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
        let work_cmd = WorkCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            false,
            JobPriority::None,
        );

        assert_eq!(work_cmd.media_root, temp_dir.path());
        assert!(!work_cmd.background_mode);
        assert_eq!(work_cmd.priority_mode, JobPriority::None);
    }

    #[tokio::test]
    async fn test_work_nonexistent_directory() {
        let work_cmd = WorkCommand::new(
            PathBuf::from("/nonexistent/path"),
            PathBuf::from("/tmp"),
            false,
            JobPriority::None,
        );

        let result = work_cmd.execute().await;
        assert!(result.is_err());
    }
}
