use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tokio::fs as async_fs;
use tracing::{debug, warn};

use crate::job::Job;

/// Manages the job queue with atomic operations for distributed processing
pub struct JobQueue {
    #[allow(dead_code)]
    pub media_root: PathBuf,
    pub queue_dir: PathBuf,
    pub in_progress_dir: PathBuf,
    pub completed_dir: PathBuf,
}

impl JobQueue {
    /// Create a new job queue with queue directory separate from media directory
    pub fn new(media_root: PathBuf, queue_root: PathBuf) -> Self {
        let queue_dir = queue_root.join("_queue");
        let in_progress_dir = queue_root.join("_in_progress");
        let completed_dir = queue_root.join("_completed");

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
        let job_content = serde_json::to_string_pretty(job)?;
        let job_filename = job.job_filename();
        let job_path = self.queue_dir.join(&job_filename);
        let lock_dir = self.queue_dir.join(format!("{job_filename}.lock"));

        // Use a lock directory for atomic job creation
        if async_fs::create_dir(&lock_dir).await.is_err() {
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
                Err(anyhow!("Failed to create job file: {e}"))
            }
        }
    }

    /// Atomically claim a job from the queue with optional prioritization
    pub async fn claim_job(
        &self,
        priority: Option<crate::JobPriority>,
    ) -> Result<Option<ClaimedJob<'_>>> {
        match priority {
            Some(crate::JobPriority::Episode) => self.claim_prioritized_job().await,
            _ => self.claim_first_available_job().await,
        }
    }

    /// Claim the first available job (original behavior)
    async fn claim_first_available_job(&self) -> Result<Option<ClaimedJob<'_>>> {
        let mut entries = async_fs::read_dir(&self.queue_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(extension) = path.extension() {
                if extension == "job" {
                    if let Some(claimed_job) = self.try_claim_job_file(&path).await? {
                        return Ok(Some(claimed_job));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Claim a job with episode prioritization
    async fn claim_prioritized_job(&self) -> Result<Option<ClaimedJob<'_>>> {
        // First, collect all available job files
        let mut job_files = Vec::new();
        let mut entries = async_fs::read_dir(&self.queue_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if let Some(extension) = path.extension() {
                if extension == "job" {
                    job_files.push(path);
                }
            }
        }

        if job_files.is_empty() {
            return Ok(None);
        }

        // Load all jobs and extract metadata for sorting
        let mut jobs_with_metadata = Vec::new();
        for job_path in job_files {
            // Try to read the job file
            if let Ok(content) = async_fs::read_to_string(&job_path).await {
                if let Ok(job) = serde_json::from_str::<Job>(&content) {
                    let metadata = job.extract_episode_metadata();
                    jobs_with_metadata.push((job_path, job, metadata));
                }
            }
        }

        // Sort jobs by priority:
        // 1. Episode jobs first (with metadata)
        // 2. Within episodes: by series name, then season, then episode
        // 3. Non-episode jobs last (maintain original order)
        jobs_with_metadata.sort_by(|a, b| {
            match (&a.2, &b.2) {
                (Some(meta_a), Some(meta_b)) => {
                    // Both have metadata - sort by series, season, episode
                    meta_a
                        .series_name
                        .cmp(&meta_b.series_name)
                        .then(meta_a.season_number.cmp(&meta_b.season_number))
                        .then(meta_a.episode_number.cmp(&meta_b.episode_number))
                }
                (Some(_), None) => std::cmp::Ordering::Less, // Episode jobs first
                (None, Some(_)) => std::cmp::Ordering::Greater, // Episode jobs first
                (None, None) => std::cmp::Ordering::Equal,   // Maintain order for non-episodes
            }
        });

        // Try to claim jobs in priority order
        for (job_path, _, _) in jobs_with_metadata {
            if let Some(claimed_job) = self.try_claim_job_file(&job_path).await? {
                return Ok(Some(claimed_job));
            }
        }

        Ok(None)
    }

    /// Try to atomically claim a specific job file
    async fn try_claim_job_file(
        &self,
        job_path: &std::path::Path,
    ) -> Result<Option<ClaimedJob<'_>>> {
        let job_name = job_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("Invalid job filename"))?;

        let in_progress_path = self.in_progress_dir.join(job_name);

        // Atomically move job from queue to in_progress
        match async_fs::rename(job_path, &in_progress_path).await {
            Ok(_) => {
                debug!("Claimed job: {}", job_name);

                // Read and deserialize job content
                let content = async_fs::read_to_string(&in_progress_path).await?;
                let job: Job = serde_json::from_str(&content)?;

                Ok(Some(ClaimedJob {
                    queue: self,
                    job_name: job_name.to_string(),
                    job,
                    in_progress_path,
                }))
            }
            Err(_) => {
                // Job was claimed by another worker
                Ok(None)
            }
        }
    }

    /// Check if a job already exists in the queue
    pub async fn job_exists(&self, job: &Job) -> Result<bool> {
        let job_filename = job.job_filename();
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
    #[allow(dead_code)]
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
    pub job: Job,
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
    #[allow(dead_code)]
    pub fn file_extension(&self) -> Option<&str> {
        self.job.input_path.extension()?.to_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
    use tempfile::TempDir;
    use tokio::test;

    #[test]
    async fn test_queue_initialization() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());

        queue.init().await.unwrap();

        assert!(queue.queue_dir.exists());
        assert!(queue.in_progress_dir.exists());
        assert!(queue.completed_dir.exists());
    }

    #[test]
    async fn test_remote_queue_initialization() {
        let media_dir = TempDir::new().unwrap();
        let queue_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(
            media_dir.path().to_path_buf(),
            queue_dir.path().to_path_buf(),
        );

        queue.init().await.unwrap();

        // Queue directories should be in the separate queue directory
        assert!(queue_dir.path().join("_queue").exists());
        assert!(queue_dir.path().join("_in_progress").exists());
        assert!(queue_dir.path().join("_completed").exists());

        // Media directory should be clean
        assert!(!media_dir.path().join("_queue").exists());
    }

    #[test]
    async fn test_job_enqueue_and_claim() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = temp_dir.path();
        let job = Job::new(
            PathBuf::from("test.webm"),
            MediaFileType::WebM,
            quality,
            post_processing,
            media_root,
        );

        // Enqueue job
        queue.enqueue_job(&job).await.unwrap();

        // Claim job
        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        assert!(claimed.job.input_path.ends_with("test.webm"));
        assert_eq!(claimed.job.file_type, MediaFileType::WebM);

        // Mark as complete
        claimed.complete().await.unwrap();

        // Should be no more jobs
        assert!(queue.claim_job(None).await.unwrap().is_none());
    }

    #[test]
    async fn test_episode_prioritization() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = temp_dir.path();

        // Create jobs in non-sorted order
        let jobs = vec![
            // Breaking Bad Season 1 (older series)
            Job::new(
                PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad S01E03 Gray Matter.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad S01E01 Pilot.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            // Better Call Saul Season 1 (newer series)
            Job::new(
                PathBuf::from("Series/Better Call Saul/Season 01/Better Call Saul S01E02 Mijo.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                PathBuf::from("Series/Better Call Saul/Season 01/Better Call Saul S01E01 Uno.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            // Non-episode job (movie)
            Job::new(
                PathBuf::from("Movies/The Matrix (1999)/The Matrix (1999).mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
        ];

        // Enqueue jobs in non-priority order
        for job in jobs {
            queue.enqueue_job(&job).await.unwrap();
        }

        // Claim jobs with episode prioritization
        let mut claimed_order = Vec::new();
        while let Some(claimed) = queue
            .claim_job(Some(crate::JobPriority::Episode))
            .await
            .unwrap()
        {
            let path_str = claimed.job.input_path.to_string_lossy().to_string();
            claimed_order.push(path_str);
            claimed.complete().await.unwrap();
        }

        // Should have 5 jobs
        assert_eq!(claimed_order.len(), 5);

        // Episodes should come first, sorted by series name then episode number
        // Better Call Saul comes before Breaking Bad alphabetically
        assert!(claimed_order[0].contains("Better Call Saul S01E01"));
        assert!(claimed_order[1].contains("Better Call Saul S01E02"));
        assert!(claimed_order[2].contains("Breaking Bad S01E01"));
        assert!(claimed_order[3].contains("Breaking Bad S01E03"));
        // Movie should come last
        assert!(claimed_order[4].contains("The Matrix"));
    }

    #[test]
    async fn test_no_prioritization() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let media_root = temp_dir.path();

        // Create a few jobs
        let jobs = vec![
            Job::new(
                PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad S01E03 Gray Matter.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                PathBuf::from("Series/Breaking Bad/Season 01/Breaking Bad S01E01 Pilot.mkv"),
                MediaFileType::Mkv,
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
        ];

        // Enqueue jobs
        for job in jobs {
            queue.enqueue_job(&job).await.unwrap();
        }

        // With no prioritization, jobs should be claimed in directory order
        let claimed1 = queue.claim_job(None).await.unwrap().unwrap();
        let claimed2 = queue.claim_job(None).await.unwrap().unwrap();

        // Both jobs should be claimed regardless of episode order
        assert!(claimed1
            .job
            .input_path
            .to_string_lossy()
            .contains("Breaking Bad"));
        assert!(claimed2
            .job
            .input_path
            .to_string_lossy()
            .contains("Breaking Bad"));

        // Clean up
        claimed1.complete().await.unwrap();
        claimed2.complete().await.unwrap();
    }
}
