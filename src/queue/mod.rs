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

    /// Add a job to the queue.
    ///
    /// The job is written under a staging name and renamed into place. Rename is
    /// atomic and the job filename is the v5 id of the input path, so two
    /// scanners racing on one file end up with one job file holding one of two
    /// identical writes - the same outcome a claim on the name would give, with
    /// nothing left behind to clean up.
    ///
    /// That last part is why the name is not claimed with a marker of its own: a
    /// scanner interrupted after taking a name and before writing the job would
    /// leave the name taken with no job under it, and every later scan would
    /// then skip a file that is in no queue directory at all.
    pub async fn enqueue_job(&self, job: &Job) -> Result<()> {
        let job_filename = job.job_filename();
        let job_path = self.queue_dir.join(&job_filename);

        write_job_atomically(&job_path, job)
            .await
            .map_err(|e| anyhow!("Failed to create job file: {e}"))?;

        // A lock directory left in the queue by an older writer is debris: names
        // are no longer claimed that way, and a job now sits beside it.
        let _ = async_fs::remove_dir(self.queue_dir.join(format!("{job_filename}.lock"))).await;

        debug!("Created job: {}", job_filename);
        Ok(())
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

                // Rename preserves mtime, so a job that waited in the queue
                // longer than STALE_AFTER - routine on any backlog - is already
                // stale the moment it is claimed. Beat once here, before the
                // job is handed to a caller, so there is no window in which it
                // is both claimed and reclaimable.
                let heartbeat_path = heartbeat_path_for(&in_progress_path);
                async_fs::write(&heartbeat_path, b"").await.map_err(|e| {
                    anyhow!("Claimed {job_name} but could not write its heartbeat: {e}")
                })?;

                // Read and deserialize job content
                let content = async_fs::read_to_string(&in_progress_path).await?;
                let job: Job = serde_json::from_str(&content)?;

                Ok(Some(ClaimedJob {
                    queue: self,
                    job_name: job_name.to_string(),
                    job,
                    in_progress_path,
                    heartbeat: Some(spawn_heartbeat(heartbeat_path)),
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
    pub async fn reclaim_stranded_jobs(&self, stale_after: Duration) -> Result<SweepOutcome> {
        let mut outcome = SweepOutcome::default();

        if !self.in_progress_dir.exists() {
            return Ok(outcome);
        }

        // Collected on the way past, and used afterwards to tell a heartbeat
        // that still belongs to a job from one whose job is long gone.
        let mut stranded_jobs = Vec::new();
        let mut heartbeats = Vec::new();

        let mut entries = async_fs::read_dir(&self.in_progress_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("job") => stranded_jobs.push(path),
                Some("heartbeat") => heartbeats.push(path),
                _ => continue,
            }
        }

        for path in &stranded_jobs {
            let Some(job_name) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };

            let heartbeat_path = heartbeat_path_for(path);
            if !is_stale(path, &heartbeat_path, stale_after).await {
                debug!("Job is still being worked on, leaving it alone: {job_name}");
                continue;
            }

            let reclaimed = match self.reclaim_one(path, &job_name).await {
                Ok(reclaimed) => reclaimed,
                Err(e) => {
                    warn!("Could not reclaim {job_name}: {e}");
                    continue;
                }
            };

            if reclaimed.moved_the_job() {
                let _ = async_fs::remove_file(&heartbeat_path).await;
            }

            match reclaimed {
                Reclaimed::Lost => debug!("Another worker got to {job_name} first"),
                Reclaimed::Counted(FailureDisposition::Requeued { .. }) => {
                    info!("♻️ Reclaimed abandoned job: {job_name}");
                    outcome.reclaimed.push(job_name);
                }
                Reclaimed::Counted(FailureDisposition::Parked { attempts }) => {
                    warn!(
                        "🚫 {job_name} has taken down {attempts} workers; parking it in _failed."
                    );
                    outcome.parked.push(job_name);
                }
                Reclaimed::Unreadable => {
                    warn!("🚫 {job_name} is not readable as a job; moving it to _failed.");
                    outcome.unreadable.push(job_name);
                }
            }
        }

        // A heartbeat write dispatched to the blocking pool can land after the
        // job it belonged to has already moved on, leaving a file nothing else
        // would ever collect. The sweep is already reading this directory.
        for heartbeat in heartbeats {
            let job_path = heartbeat.with_extension("");
            if !stranded_jobs.contains(&job_path) {
                let _ = async_fs::remove_file(&heartbeat).await;
                debug!("Removed heartbeat with no job: {heartbeat:?}");
            }
        }

        Ok(outcome)
    }

    /// Move one abandoned job out of `_in_progress`, counting the attempt.
    ///
    /// A worker that a job took down with it - an encode that ran the machine
    /// out of memory, a panic, a wedged driver - never reaches
    /// [`ClaimedJob::fail`], so without counting here such a job would cycle
    /// forever, at the cost of a worker each time round rather than ten seconds.
    /// It is the same loop `_failed` exists to break, so it ends the same way.
    ///
    /// [`Reclaimed::Lost`] means another worker got there first, which is not an
    /// error: the move is a rename, and the loser of a race simply does nothing.
    async fn reclaim_one(
        &self,
        in_progress_path: &std::path::Path,
        job_name: &str,
    ) -> Result<Reclaimed> {
        let content = async_fs::read_to_string(in_progress_path).await?;

        // A read that failed is worth another sweep - a network work root drops
        // out from under a worker now and then - but contents that are not a job
        // will not become one, and nothing else ever looks in `_in_progress`.
        let mut job: Job = match serde_json::from_str(&content) {
            Ok(job) => job,
            Err(e) => {
                return self
                    .quarantine_unreadable(in_progress_path, job_name, &e.to_string())
                    .await
            }
        };

        job.attempts += 1;
        job.last_error = Some(format!(
            "the worker running this job stopped without finishing it (attempt {})",
            job.attempts
        ));

        let attempts = job.attempts;
        let park = attempts >= MAX_ATTEMPTS;

        // The count goes in under a staging name and is renamed over the job, so
        // a sweeper interrupted here leaves a job file that still parses.
        write_job_atomically(in_progress_path, &job).await?;

        let destination = if park {
            self.failed_dir.join(job_name)
        } else {
            self.queue_dir.join(job_name)
        };
        if let Some(parent) = destination.parent() {
            async_fs::create_dir_all(parent).await?;
        }

        match async_fs::rename(in_progress_path, &destination).await {
            Ok(_) if park => Ok(Reclaimed::Counted(FailureDisposition::Parked { attempts })),
            Ok(_) => Ok(Reclaimed::Counted(FailureDisposition::Requeued {
                attempts,
            })),
            Err(_) => Ok(Reclaimed::Lost),
        }
    }

    /// Move a job file that cannot be read as a job out of `_in_progress`.
    ///
    /// Contents that are not a job are not going to become one, so a sweep that
    /// only warned about them would warn on every worker startup forever, while
    /// `job_exists` - which goes by filename - kept the media file out of the
    /// queue for just as long. `_failed` is where a person already looks for a
    /// job that needs a decision, and the parse error goes beside it.
    async fn quarantine_unreadable(
        &self,
        in_progress_path: &std::path::Path,
        job_name: &str,
        reason: &str,
    ) -> Result<Reclaimed> {
        async_fs::create_dir_all(&self.failed_dir).await?;
        let destination = self.failed_dir.join(job_name);

        // A rename, like every other move between these directories, so two
        // sweepers cannot both take the same file.
        if async_fs::rename(in_progress_path, &destination)
            .await
            .is_err()
        {
            return Ok(Reclaimed::Lost);
        }

        let mut note = destination.into_os_string();
        note.push(".error");
        let _ = async_fs::write(PathBuf::from(note), reason.as_bytes()).await;

        Ok(Reclaimed::Unreadable)
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

/// Write a job file so that it is never observed half-written.
///
/// The contents go to a staging name of their own and are renamed onto the job
/// path, which replaces whatever is there in one step. A writer killed part-way
/// through therefore leaves either the previous job file or nothing - never a
/// torn file that no later run can parse - and the staging name carries a v4
/// uuid so that two writers cannot tear each other's copy.
async fn write_job_atomically(job_path: &std::path::Path, job: &Job) -> Result<()> {
    let content = serde_json::to_string_pretty(job)?;

    let mut staging = job_path.as_os_str().to_os_string();
    staging.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let staging = PathBuf::from(staging);

    if let Err(e) = async_fs::write(&staging, content.as_bytes()).await {
        let _ = async_fs::remove_file(&staging).await;
        return Err(anyhow!("could not write {staging:?}: {e}"));
    }

    if let Err(e) = async_fs::rename(&staging, job_path).await {
        let _ = async_fs::remove_file(&staging).await;
        return Err(anyhow!("could not move {staging:?} into place: {e}"));
    }

    Ok(())
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

/// What a startup sweep of `_in_progress` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Jobs put back in `_queue` for another worker to pick up.
    pub reclaimed: Vec<String>,
    /// Jobs that have now taken down `MAX_ATTEMPTS` workers and were parked.
    pub parked: Vec<String>,
    /// Job files that could not be read as jobs and were moved to `_failed`.
    pub unreadable: Vec<String>,
}

impl SweepOutcome {
    /// Whether the sweep moved anything at all.
    pub fn is_empty(&self) -> bool {
        self.reclaimed.is_empty() && self.parked.is_empty() && self.unreadable.is_empty()
    }
}

/// What the sweep did with one file in `_in_progress`.
enum Reclaimed {
    /// The attempt was counted and the job moved on, to `_queue` or `_failed`.
    Counted(FailureDisposition),
    /// The file could not be read as a job and was moved to `_failed`.
    Unreadable,
    /// Another sweeper moved the file first; this one did nothing.
    Lost,
}

impl Reclaimed {
    /// Whether this sweep is the one that moved the file out of `_in_progress`,
    /// and so the one that owns whatever was left beside it.
    fn moved_the_job(&self) -> bool {
        !matches!(self, Reclaimed::Lost)
    }
}

/// Keep refreshing a heartbeat until the task is aborted.
fn spawn_heartbeat(path: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            if let Err(e) = async_fs::write(&path, b"").await {
                warn!("Could not refresh job heartbeat: {e}");
            }
        }
    })
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
    /// Refreshes the heartbeat for as long as this claim is held.
    ///
    /// Owned here rather than by the caller so that holding a `ClaimedJob` is
    /// what protects the job, with no second thing for a caller to remember.
    /// Dropping the claim - which is what cancelling a worker mid-encode does -
    /// stops the refresh, and the sweep takes the job back a few minutes later.
    heartbeat: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ClaimedJob<'_> {
    fn drop(&mut self) {
        self.stop_heartbeat();
    }
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
    pub async fn complete(mut self) -> Result<()> {
        self.stop_heartbeat();
        let completed_path = self.queue.completed_dir.join(&self.job_name);
        async_fs::rename(&self.in_progress_path, completed_path).await?;
        let _ = async_fs::remove_file(heartbeat_path_for(&self.in_progress_path)).await;
        debug!("Marked job as completed: {}", self.job_name);
        Ok(())
    }

    /// Stop refreshing the heartbeat.
    ///
    /// Aborting cannot recall a write already dispatched to the blocking pool,
    /// so a heartbeat can outlive the job file it belonged to. The sweep clears
    /// those up; see [`JobQueue::reclaim_stranded_jobs`].
    fn stop_heartbeat(&mut self) {
        if let Some(handle) = self.heartbeat.take() {
            handle.abort();
        }
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
        self.stop_heartbeat();
        self.job.attempts += 1;
        // Keep the tail of the message: that is where FFmpeg says what actually
        // went wrong, and a job file is no place for a megabyte of log.
        self.job.last_error = Some(tail(error, 2000));

        let attempts = self.job.attempts;
        let park = attempts >= MAX_ATTEMPTS;

        // Rewrite in place first, then move: the move stays a rename, which is
        // what keeps two workers from both getting the job. The rewrite is
        // itself a rename onto the job file, so a worker killed here leaves a
        // job the next sweep can still read.
        write_job_atomically(&self.in_progress_path, &self.job).await?;

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
    ///
    /// Dropping the claim is what an interrupted worker does: the job file
    /// stays in `_in_progress` and the heartbeat stops being refreshed.
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

        let job_name = {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed.job_name().to_string()
        };

        (queue, job_name)
    }

    /// The heartbeat file that belongs to a claimed job.
    fn heartbeat_of(in_progress_path: &Path) -> PathBuf {
        let mut name = in_progress_path.as_os_str().to_os_string();
        name.push(".heartbeat");
        PathBuf::from(name)
    }

    /// Backdate a file's modification time, so the sweep sees it as old.
    ///
    /// Every failure here is unwrapped. A helper that quietly does nothing
    /// would leave the tests below asserting that a *fresh* job is not
    /// reclaimed, which they would pass without testing anything.
    fn age(paths: &[&Path], by: Duration) {
        let when = std::fs::FileTimes::new().set_modified(SystemTime::now() - by);
        for path in paths {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_times(when).unwrap();
        }
    }

    #[test]
    async fn an_abandoned_job_is_returned_to_the_queue() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        assert!(in_progress_path.exists(), "the job starts out claimed");

        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(600),
        );

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(swept.reclaimed, vec![job_name.clone()]);
        assert!(swept.parked.is_empty());
        assert!(!in_progress_path.exists());
        assert!(queue.queue_dir.join(&job_name).exists());

        // And a worker can pick it up again, which is the whole point.
        assert!(queue.claim_job(None).await.unwrap().is_some());
    }

    #[test]
    async fn a_job_a_worker_is_still_running_is_left_alone() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(swept.is_empty());
        assert!(queue.in_progress_dir.join(&job_name).exists());
    }

    #[test]
    async fn a_job_claimed_after_a_long_wait_is_not_immediately_reclaimable() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("waited.mkv"),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();

        // The job sat in the queue behind a backlog for an hour. Claiming is a
        // rename, and rename keeps the mtime, so the job arrives in
        // `_in_progress` already older than any threshold.
        age(
            &[&queue.queue_dir.join(job.job_filename())],
            Duration::from_secs(3600),
        );

        let claimed = queue.claim_job(None).await.unwrap().unwrap();

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(
            swept.is_empty(),
            "claiming has to protect the job, not just move it"
        );
        drop(claimed);
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
        age(&[&in_progress_path], Duration::from_secs(6000));

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(
            swept.is_empty(),
            "a job whose worker is still checking in must not be taken away"
        );
        assert!(in_progress_path.exists());

        // Once the worker stops checking in, the job comes back.
        drop(claimed);
        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(6000),
        );
        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();
        assert_eq!(swept.reclaimed, vec![job_name.clone()]);
        assert!(
            !heartbeat_of(&in_progress_path).exists(),
            "the stale heartbeat is cleared away with the job"
        );
    }

    #[test]
    async fn a_job_that_keeps_taking_its_worker_down_is_parked_too() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        // A job that kills the worker never reaches `fail`, so if the sweep did
        // not count the attempt this loop would run forever - at the cost of a
        // worker each time round.
        for attempt in 1..MAX_ATTEMPTS {
            let in_progress_path = queue.in_progress_dir.join(&job_name);
            age(
                &[&in_progress_path, &heartbeat_of(&in_progress_path)],
                Duration::from_secs(600),
            );

            let swept = queue
                .reclaim_stranded_jobs(Duration::from_secs(300))
                .await
                .unwrap();
            assert_eq!(swept.reclaimed, vec![job_name.clone()]);

            let requeued: Job = serde_json::from_str(
                &std::fs::read_to_string(queue.queue_dir.join(&job_name)).unwrap(),
            )
            .unwrap();
            assert_eq!(requeued.attempts, attempt);

            drop(queue.claim_job(None).await.unwrap().unwrap());
        }

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(600),
        );
        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(swept.reclaimed.is_empty());
        assert_eq!(swept.parked, vec![job_name.clone()]);
        assert!(queue.failed_dir.join(&job_name).exists());

        let parked: Job = serde_json::from_str(
            &std::fs::read_to_string(queue.failed_dir.join(&job_name)).unwrap(),
        )
        .unwrap();
        assert_eq!(parked.attempts, MAX_ATTEMPTS);
        assert!(parked
            .last_error
            .as_deref()
            .unwrap()
            .contains("stopped without finishing"));
    }

    #[test]
    async fn a_heartbeat_left_behind_by_a_finished_job_is_collected() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        // Aborting the refresh task cannot recall a write already dispatched to
        // the blocking pool, so a heartbeat can outlive its job. Nothing else
        // walks `_in_progress`, so left alone it would sit there forever.
        let orphan = queue
            .in_progress_dir
            .join("00000000-dead-beef.job.heartbeat");
        std::fs::write(&orphan, b"").unwrap();

        queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(!orphan.exists());
    }

    #[test]
    async fn a_heartbeat_belonging_to_a_running_job_is_kept() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        let heartbeat = heartbeat_of(&queue.in_progress_dir.join(&job_name));
        assert!(
            heartbeat.exists(),
            "collecting orphans must not take a live worker's heartbeat"
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
    async fn a_lock_left_by_an_interrupted_scan_does_not_keep_a_file_out_of_the_queue() {
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

        // A scan that stopped after taking the name and before writing the job.
        // Nothing in any queue directory records the file, so no later run has
        // any way to notice it was missed.
        let lock_dir = queue.queue_dir.join(format!("{}.lock", job.job_filename()));
        std::fs::create_dir(&lock_dir).unwrap();
        assert!(!queue.job_exists(&job).await.unwrap());

        queue.enqueue_job(&job).await.unwrap();

        assert!(
            queue.queue_dir.join(job.job_filename()).exists(),
            "a name taken by a scan that died must not be taken forever"
        );
        assert!(!lock_dir.exists(), "the debris is cleared away with it");

        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        assert!(claimed.job.input_path.ends_with("show.mkv"));
    }

    #[test]
    async fn two_scans_of_one_library_produce_one_job_per_file() {
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

        // Two scanners on one work root, writing the same file at the same time.
        let (first, second) = tokio::join!(queue.enqueue_job(&job), queue.enqueue_job(&job));
        first.unwrap();
        second.unwrap();

        let mut job_files = Vec::new();
        let mut entries = std::fs::read_dir(&queue.queue_dir).unwrap();
        while let Some(Ok(entry)) = entries.next() {
            job_files.push(entry.file_name().to_string_lossy().to_string());
        }

        assert_eq!(
            job_files,
            vec![job.job_filename()],
            "one file means one job, and no staging debris beside it"
        );

        // The one job that survived is a whole one, not two interleaved writes.
        let queued: Job = serde_json::from_str(
            &std::fs::read_to_string(queue.queue_dir.join(job.job_filename())).unwrap(),
        )
        .unwrap();
        assert_eq!(queued.id, job.id);
    }

    #[test]
    async fn a_job_file_that_cannot_be_read_is_moved_out_of_the_way() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // A job file that is not a job - a torn write from an older worker, or a
        // network work root that dropped out mid-rewrite. Nothing but the sweep
        // ever looks in `_in_progress`, so left in place it is warned about at
        // every startup and never resolved.
        std::fs::write(&in_progress_path, b"{\"id\": \"half a job").unwrap();
        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(600),
        );

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(swept.unreadable, vec![job_name.clone()]);
        assert!(swept.reclaimed.is_empty() && swept.parked.is_empty());
        assert!(!in_progress_path.exists());
        assert!(queue.failed_dir.join(&job_name).exists());
        assert!(
            !heartbeat_of(&in_progress_path).exists(),
            "the heartbeat goes with the job it belonged to"
        );

        // Why it could not be read is recorded where a person will find it.
        let note = std::fs::read_to_string(queue.failed_dir.join(format!("{job_name}.error")))
            .expect("the parse error is written beside the job");
        assert!(!note.is_empty());

        // And the next worker has nothing left to warn about.
        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();
        assert!(swept.is_empty());
    }

    #[test]
    async fn an_interrupted_rewrite_leaves_the_job_file_readable() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // Recording an attempt rewrites the job file. A worker killed in the
        // middle of that leaves the half-written copy under the staging name it
        // was writing to, and the job itself as it was.
        let mut staging = in_progress_path.as_os_str().to_os_string();
        staging.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let staging = PathBuf::from(staging);
        std::fs::write(&staging, b"{\"id\": \"half a job").unwrap();

        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(600),
        );

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(
            swept.reclaimed,
            vec![job_name.clone()],
            "a torn write must not cost the job its place in the queue"
        );

        let requeued: Job = serde_json::from_str(
            &std::fs::read_to_string(queue.queue_dir.join(&job_name)).unwrap(),
        )
        .unwrap();
        assert_eq!(requeued.attempts, 1);
        assert!(
            swept.parked.is_empty() && swept.unreadable.is_empty(),
            "the sweep judges jobs, and a staging file is not one"
        );
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
