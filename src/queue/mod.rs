use anyhow::{Result, anyhow};
use std::path::PathBuf;
use tracing::{debug, warn};
use tokio::fs as async_fs;

use crate::job::Job;

/// Manages the job queue with atomic operations for distributed processing
pub struct JobQueue {
    pub media_root: PathBuf,
    pub queue_dir: PathBuf,
    pub in_progress_dir: PathBuf,
    pub completed_dir: PathBuf,
}

impl JobQueue {
    /// Create a new job queue for the given media directory
    pub fn new(media_root: PathBuf) -> Self {
        let queue_dir = media_root.join("_queue");
        let in_progress_dir = media_root.join("_in_progress");
        let completed_dir = media_root.join("_completed");

        Self {
            media_root,
            queue_dir,
            in_progress_dir,
            completed_dir,
        }
    }

    /// Initialize queue directories
    pub async fn init(&self) -> Result<()> {
        async_fs::create_dir_all(&self.queue_dir).await?;
        async_fs::create_dir_all(&self.in_progress_dir).await?;
        async_fs::create_dir_all(&self.completed_dir).await?;
        Ok(())
    }

    /// Add a job to the queue using atomic file operations
    pub async fn enqueue_job(&self, job: &Job) -> Result<()> {
        let job_content = job.relative_path.to_string_lossy();
        let job_filename = job.job_filename_from_source();
        let job_path = self.queue_dir.join(&job_filename);
        let lock_dir = self.queue_dir.join(format!("{}.lock", job_filename));

        // Use a lock directory for atomic job creation
        if let Err(_) = async_fs::create_dir(&lock_dir).await {
            debug!("Job already being created: {}", job_filename);
            return Ok(()); // Job is already being created by another process
        }

        // Write job file
        match async_fs::write(&job_path, job_content.as_bytes()).await {
            Ok(_) => {
                debug!("Created job: {}", job_filename);
                // Remove lock directory
                let _ = async_fs::remove_dir(&lock_dir).await;
                Ok(())
            }
            Err(e) => {
                // Clean up lock directory on error
                let _ = async_fs::remove_dir(&lock_dir).await;
                Err(anyhow!("Failed to create job file: {}", e))
            }
        }
    }

    /// Atomically claim a job from the queue
    pub async fn claim_job(&self) -> Result<Option<ClaimedJob>> {
        let mut entries = async_fs::read_dir(&self.queue_dir).await?;
        
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(extension) = path.extension() {
                if extension == "job" {
                    let job_name = path.file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| anyhow!("Invalid job filename"))?;
                    
                    let in_progress_path = self.in_progress_dir.join(job_name);
                    
                    // Atomically move job from queue to in_progress
                    match async_fs::rename(&path, &in_progress_path).await {
                        Ok(_) => {
                            debug!("Claimed job: {}", job_name);
                            
                            // Read job content
                            let content = async_fs::read_to_string(&in_progress_path).await?;
                            let relative_path = PathBuf::from(content.trim());
                            
                            return Ok(Some(ClaimedJob {
                                queue: self,
                                job_name: job_name.to_string(),
                                relative_path,
                                in_progress_path,
                            }));
                        }
                        Err(_) => {
                            // Job was claimed by another worker, continue
                            continue;
                        }
                    }
                }
            }
        }
        
        Ok(None)
    }

    /// Check if a job already exists in the queue
    pub async fn job_exists(&self, job: &Job) -> Result<bool> {
        let job_filename = job.job_filename_from_source();
        let job_path = self.queue_dir.join(job_filename);
        Ok(job_path.exists())
    }

    /// Clean up all queue directories
    pub async fn clean(&self) -> Result<()> {
        if self.queue_dir.exists() {
            async_fs::remove_dir_all(&self.queue_dir).await?;
        }
        if self.in_progress_dir.exists() {
            async_fs::remove_dir_all(&self.in_progress_dir).await?;
        }
        if self.completed_dir.exists() {
            async_fs::remove_dir_all(&self.completed_dir).await?;
        }
        Ok(())
    }

    /// Get count of pending jobs
    pub async fn pending_count(&self) -> Result<usize> {
        let mut count = 0;
        let mut entries = async_fs::read_dir(&self.queue_dir).await?;
        
        while let Some(entry) = entries.next_entry().await? {
            if let Some(extension) = entry.path().extension() {
                if extension == "job" {
                    count += 1;
                }
            }
        }
        
        Ok(count)
    }
}

/// Represents a job that has been claimed by a worker
pub struct ClaimedJob<'a> {
    queue: &'a JobQueue,
    job_name: String,
    pub relative_path: PathBuf,
    in_progress_path: PathBuf,
}

impl<'a> ClaimedJob<'a> {
    /// Mark the job as completed
    pub async fn complete(self) -> Result<()> {
        let completed_path = self.queue.completed_dir.join(&self.job_name);
        async_fs::rename(&self.in_progress_path, completed_path).await?;
        debug!("Marked job as completed: {}", self.job_name);
        Ok(())
    }

    /// Return the job to the queue (e.g., on failure)
    pub async fn return_to_queue(self) -> Result<()> {
        let queue_path = self.queue.queue_dir.join(&self.job_name);
        async_fs::rename(&self.in_progress_path, queue_path).await?;
        warn!("Returned job to queue: {}", self.job_name);
        Ok(())
    }

    /// Get the job name
    pub fn job_name(&self) -> &str {
        &self.job_name
    }

    /// Get the media file extension
    pub fn file_extension(&self) -> Option<&str> {
        self.relative_path.extension()?.to_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::test;

    #[test]
    async fn test_queue_initialization() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf());
        
        queue.init().await.unwrap();
        
        assert!(queue.queue_dir.exists());
        assert!(queue.in_progress_dir.exists());
        assert!(queue.completed_dir.exists());
    }
}