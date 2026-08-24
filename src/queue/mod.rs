use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::fs as async_fs;
use tracing::{debug, info, warn};

use crate::job::Job;

/// How many times a job may fail before it is parked in `_failed` instead of
/// being handed back to the queue.
///
/// A job that cannot succeed - a source FFmpeg cannot decode, a missing
/// FFmpeg, an unreadable file - would otherwise be claimed, fail, and be
/// requeued forever, and a worker spinning on it makes no progress on the rest
/// of the queue.
pub const MAX_ATTEMPTS: u32 = 3;

/// How long a claimed job may go without a heartbeat before another worker may
/// take it back.
///
/// A worker touches its heartbeat file every `HEARTBEAT_INTERVAL`, so this only
/// elapses for a worker that is gone.
pub const STALE_AFTER: Duration = Duration::from_secs(300);

/// How often a worker refreshes the heartbeat of the job it is running.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Manages the job queue with atomic operations for distributed processing
pub struct JobQueue {
    #[allow(dead_code)]
    pub media_root: PathBuf,
    pub queue_dir: PathBuf,
    pub in_progress_dir: PathBuf,
    pub completed_dir: PathBuf,
    pub failed_dir: PathBuf,
}

impl JobQueue {
    /// Create a new job queue with queue directory separate from media directory
    pub fn new(media_root: PathBuf, queue_root: PathBuf) -> Self {
        let queue_dir = queue_root.join("_queue");
        let in_progress_dir = queue_root.join("_in_progress");
        let completed_dir = queue_root.join("_completed");
        let failed_dir = queue_root.join("_failed");

        Self {
            media_root,
            queue_dir,
            in_progress_dir,
            completed_dir,
            failed_dir,
        }
    }

    /// Initialize queue directories
    pub async fn init(&self) -> Result<()> {
        async_fs::create_dir_all(&self.queue_dir).await?;
        async_fs::create_dir_all(&self.in_progress_dir).await?;
        async_fs::create_dir_all(&self.completed_dir).await?;
        async_fs::create_dir_all(&self.failed_dir).await?;
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

    /// Check whether this file already has a job waiting, being worked on, or
    /// parked as unprocessable.
    ///
    /// `_completed` is deliberately not consulted. A finished job means the file
    /// was transcoded, and the source is gone from the scan once it has been
    /// disabled; if the output was later deleted, the file should be queued again
    /// rather than remembered as done forever.
    ///
    /// `_failed` is consulted, for the opposite reason. A parked job is a file
    /// this worker could not process, and re-scanning the library must not walk
    /// it back into the queue to fail another three times. Moving the job file
    /// out of `_failed` by hand is what asks for it to be tried again.
    pub async fn job_exists(&self, job: &Job) -> Result<bool> {
        let job_filename = job.job_filename();
        Ok(self.queue_dir.join(&job_filename).exists()
            || self.in_progress_dir.join(&job_filename).exists()
            || self.failed_dir.join(&job_filename).exists())
    }

    /// Return jobs abandoned in `_in_progress` to the queue.
    ///
    /// A worker that is interrupted - Ctrl-C mid-encode, a kill, a machine
    /// losing power - leaves the job file it claimed sitting in `_in_progress`,
    /// where nothing else would ever look for it. This sweep is what brings it
    /// back, and it runs at worker startup.
    ///
    /// Several workers may share one work root, so the sweep must not take a job
    /// away from a worker that is still running it. Each running worker refreshes
    /// a heartbeat file beside its job (see [`ClaimedJob::heartbeat`]), and only
    /// a job whose heartbeat - or, for a job claimed before this existed, whose
    /// own file - has not been touched for `stale_after` is reclaimed.
    ///
    /// Whatever the interrupted encode left in the work folder is left where it
    /// is: the encoder decides on the next attempt what of it is still usable.
    pub async fn reclaim_stranded_jobs(&self, stale_after: Duration) -> Result<Vec<String>> {
        let mut reclaimed = Vec::new();

        if !self.in_progress_dir.exists() {
            return Ok(reclaimed);
        }

        let mut entries = async_fs::read_dir(&self.in_progress_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "job") {
                continue;
            }

            let Some(job_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };

            let heartbeat_path = heartbeat_path_for(&path);
            if !is_stale(&path, &heartbeat_path, stale_after).await {
                debug!("Job is still being worked on, leaving it alone: {job_name}");
                continue;
            }

            // Rename, exactly as claiming does: if another worker reclaims it
            // first, or picks it up again, the loser simply sees an error here.
            let queue_path = self.queue_dir.join(&job_name);
            match async_fs::rename(&path, &queue_path).await {
                Ok(_) => {
                    let _ = async_fs::remove_file(&heartbeat_path).await;
                    info!("♻️ Reclaimed abandoned job: {job_name}");
                    reclaimed.push(job_name);
                }
                Err(e) => {
                    debug!("Could not reclaim {job_name}: {e}");
                }
            }
        }

        Ok(reclaimed)
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
        if self.failed_dir.exists() {
            async_fs::remove_dir_all(&self.failed_dir).await?;
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

/// The last `limit` characters of a message, marked as truncated if anything
/// was dropped.
fn tail(message: &str, limit: usize) -> String {
    let skipped = message.chars().count().saturating_sub(limit);
    if skipped == 0 {
        return message.to_string();
    }

    let kept: String = message.chars().skip(skipped).collect();
    format!("[...{skipped} characters omitted...]{kept}")
}

/// The heartbeat file that sits beside a claimed job.
///
/// It is a separate file rather than a touch of the job file itself so that a
/// worker killed mid-heartbeat cannot leave a half-written job behind.
fn heartbeat_path_for(in_progress_path: &std::path::Path) -> PathBuf {
    let mut name = in_progress_path.as_os_str().to_os_string();
    name.push(".heartbeat");
    PathBuf::from(name)
}

/// Whether a claimed job has gone quiet for longer than `stale_after`.
///
/// The most recent of the two timestamps wins, so a job claimed by a worker
/// that has not yet written its first heartbeat is judged by its own mtime.
/// A timestamp that cannot be read at all is treated as not stale: refusing to
/// reclaim is always the safe answer.
async fn is_stale(
    job_path: &std::path::Path,
    heartbeat_path: &std::path::Path,
    stale_after: Duration,
) -> bool {
    let mut latest: Option<SystemTime> = None;

    for candidate in [job_path, heartbeat_path] {
        if let Ok(metadata) = async_fs::metadata(candidate).await {
            if let Ok(modified) = metadata.modified() {
                latest = Some(match latest {
                    Some(current) if current > modified => current,
                    _ => modified,
                });
            }
        }
    }

    match latest.and_then(|t| SystemTime::now().duration_since(t).ok()) {
        Some(age) => age >= stale_after,
        None => false,
    }
}

/// Represents a job that has been claimed by a worker
pub struct ClaimedJob<'a> {
    queue: &'a JobQueue,
    job_name: String,
    pub job: Job,
    in_progress_path: PathBuf,
}

/// Where a failed job went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// The job went back to `_queue` and will be tried again.
    Requeued { attempts: u32 },
    /// The job has failed too often and was parked in `_failed`.
    Parked { attempts: u32 },
}

impl<'a> ClaimedJob<'a> {
    /// Mark the job as completed
    pub async fn complete(self) -> Result<()> {
        let completed_path = self.queue.completed_dir.join(&self.job_name);
        async_fs::rename(&self.in_progress_path, completed_path).await?;
        let _ = async_fs::remove_file(heartbeat_path_for(&self.in_progress_path)).await;
        debug!("Marked job as completed: {}", self.job_name);
        Ok(())
    }

    /// Record a failed attempt and move the job on.
    ///
    /// The attempt count lives in the job file rather than in its name, because
    /// the name is the v5 id of the file being transcoded and is what makes the
    /// queue addressable. Counting in the file also means the count survives a
    /// worker restart, and gives the error somewhere to be written down.
    ///
    /// After [`MAX_ATTEMPTS`] the job is parked in `_failed` instead of being
    /// handed back, so a job that can never succeed stops holding the worker up.
    pub async fn fail(mut self, error: &str) -> Result<FailureDisposition> {
        self.job.attempts += 1;
        // Keep the tail of the message: that is where FFmpeg says what actually
        // went wrong, and a job file is no place for a megabyte of log.
        self.job.last_error = Some(tail(error, 2000));

        let attempts = self.job.attempts;
        let park = attempts >= MAX_ATTEMPTS;

        // Rewrite in place first, then move: the move stays a rename, which is
        // what keeps two workers from both getting the job.
        let content = serde_json::to_string_pretty(&self.job)?;
        async_fs::write(&self.in_progress_path, content.as_bytes()).await?;

        let destination = if park {
            self.queue.failed_dir.join(&self.job_name)
        } else {
            self.queue.queue_dir.join(&self.job_name)
        };

        if let Some(parent) = destination.parent() {
            async_fs::create_dir_all(parent).await?;
        }
        async_fs::rename(&self.in_progress_path, &destination).await?;
        let _ = async_fs::remove_file(heartbeat_path_for(&self.in_progress_path)).await;

        if park {
            // FFmpeg's complaint runs to a screenful. The job file keeps all of
            // it; the log gets the first line of it.
            let summary = error.lines().next().unwrap_or_default();
            warn!(
                "🚫 Parking job in _failed after {attempts} attempts: {} ({summary})",
                self.job_name
            );
            Ok(FailureDisposition::Parked { attempts })
        } else {
            warn!(
                "Returned job to queue (attempt {attempts} of {MAX_ATTEMPTS}): {}",
                self.job_name
            );
            Ok(FailureDisposition::Requeued { attempts })
        }
    }

    /// Refresh the heartbeat that tells other workers this job is still running.
    ///
    /// Without it, [`JobQueue::reclaim_stranded_jobs`] could not tell a worker
    /// part-way through a two-hour encode from one that died an hour ago.
    pub async fn heartbeat(&self) -> Result<()> {
        let path = heartbeat_path_for(&self.in_progress_path);
        async_fs::write(&path, b"").await?;
        Ok(())
    }

    /// The path of this job's heartbeat file, for a worker that wants to refresh
    /// it from a background task.
    pub fn heartbeat_path(&self) -> PathBuf {
        heartbeat_path_for(&self.in_progress_path)
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
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::test;

    /// Build a queue with one job already claimed, ready to be aged.
    async fn queue_with_one_claimed_job(temp_dir: &TempDir) -> (JobQueue, String) {
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();

        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        let job_name = claimed.job_name().to_string();
        // Leak the claim the way an interrupted worker does: no complete, no
        // fail, the job file simply stays where it is.
        std::mem::forget(claimed);

        (queue, job_name)
    }

    /// The heartbeat file that belongs to a claimed job.
    fn heartbeat_of(in_progress_path: &Path) -> PathBuf {
        let mut name = in_progress_path.as_os_str().to_os_string();
        name.push(".heartbeat");
        PathBuf::from(name)
    }

    /// Backdate a file's modification time, so the sweep sees it as old.
    fn age(paths: &[&Path], by: Duration) {
        let when = std::fs::FileTimes::new().set_modified(SystemTime::now() - by);
        for path in paths {
            if let Ok(file) = std::fs::File::options().write(true).open(path) {
                file.set_times(when).unwrap();
            }
        }
    }

    #[test]
    async fn an_abandoned_job_is_returned_to_the_queue() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        assert!(in_progress_path.exists(), "the job starts out claimed");

        age(&[&in_progress_path], Duration::from_secs(600));

        let reclaimed = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(reclaimed, vec![job_name.clone()]);
        assert!(!in_progress_path.exists());
        assert!(queue.queue_dir.join(&job_name).exists());

        // And a worker can pick it up again, which is the whole point.
        assert!(queue.claim_job(None).await.unwrap().is_some());
    }

    #[test]
    async fn a_job_a_worker_is_still_running_is_left_alone() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let reclaimed = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(reclaimed.is_empty());
        assert!(queue.in_progress_dir.join(&job_name).exists());
    }

    #[test]
    async fn a_long_encode_keeps_its_job_by_beating_its_heart() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("long-film.mkv"),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();

        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        let job_name = claimed.job_name().to_string();
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // The encode has been running far longer than the threshold, so the job
        // file itself is stale and only the heartbeat says the worker is alive.
        claimed.heartbeat().await.unwrap();
        age(&[&in_progress_path], Duration::from_secs(6000));

        let reclaimed = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(
            reclaimed.is_empty(),
            "a job whose worker is still checking in must not be taken away"
        );
        assert!(in_progress_path.exists());

        // Once the worker stops checking in, the job comes back.
        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(6000),
        );
        let reclaimed = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();
        assert_eq!(reclaimed, vec![job_name.clone()]);
        assert!(
            !heartbeat_of(&in_progress_path).exists(),
            "the stale heartbeat is cleared away with the job"
        );
    }

    #[test]
    async fn a_failing_job_is_retried_and_then_parked() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("corrupt.mkv"),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();
        let job_name = job.job_filename();

        for attempt in 1..MAX_ATTEMPTS {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            assert_eq!(
                claimed.fail("ffmpeg not found").await.unwrap(),
                FailureDisposition::Requeued { attempts: attempt }
            );
            assert!(
                queue.queue_dir.join(&job_name).exists(),
                "a job with attempts left goes back to the queue"
            );
        }

        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        assert_eq!(
            claimed.fail("ffmpeg not found").await.unwrap(),
            FailureDisposition::Parked {
                attempts: MAX_ATTEMPTS
            }
        );

        assert!(!queue.queue_dir.join(&job_name).exists());
        assert!(!queue.in_progress_dir.join(&job_name).exists());
        assert!(queue.failed_dir.join(&job_name).exists());

        // The parked job says how often it failed, and why.
        let parked: Job = serde_json::from_str(
            &std::fs::read_to_string(queue.failed_dir.join(&job_name)).unwrap(),
        )
        .unwrap();
        assert_eq!(parked.attempts, MAX_ATTEMPTS);
        assert_eq!(parked.last_error.as_deref(), Some("ffmpeg not found"));

        // And a re-scan does not walk it straight back into the queue.
        assert!(queue.job_exists(&job).await.unwrap());
    }

    #[test]
    async fn a_job_file_written_before_attempts_existed_still_loads() {
        // Job files sit on disk between releases; one written by an older
        // version carries no attempt count and must still deserialize.
        let legacy = r#"{
            "id": "b0a1c2d3-0000-0000-0000-000000000000",
            "input_path": "/media/show.mkv",
            "output_path": "/media/show.mp4",
            "subtitle_path": null,
            "file_type": "Mkv",
            "quality_settings": {
                "ffmpeg_preset": "veryfast",
                "ffmpeg_crf": "23",
                "ffmpeg_audio_bitrate": "128k"
            },
            "post_processing": { "disable_source_files": true }
        }"#;

        let job: Job = serde_json::from_str(legacy).unwrap();
        assert_eq!(job.attempts, 0);
        assert_eq!(job.last_error, None);
    }

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

    #[test]
    async fn test_job_exists_covers_in_progress_jobs() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );

        assert!(!queue.job_exists(&job).await.unwrap());

        queue.enqueue_job(&job).await.unwrap();
        assert!(queue.job_exists(&job).await.unwrap());

        // A worker claims it: the file leaves _queue, but re-scanning the library
        // must not queue the same file a second time behind the worker's back.
        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        assert!(
            queue.job_exists(&job).await.unwrap(),
            "a job being worked on still exists"
        );

        // Once finished, the job is no longer outstanding.
        claimed.complete().await.unwrap();
        assert!(!queue.job_exists(&job).await.unwrap());
    }
}
