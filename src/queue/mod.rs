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

/// One of the four directories a job moves between.
///
/// Named so that a caller can talk about a queue directory without holding a
/// path, and so that `clean --only` has something to parse into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, clap::ValueEnum)]
pub enum QueueDirectory {
    /// `_queue`: jobs waiting for a worker.
    Queue,
    /// `_in_progress`: jobs a worker has claimed.
    InProgress,
    /// `_completed`: jobs that finished.
    Completed,
    /// `_failed`: jobs parked after `MAX_ATTEMPTS`.
    Failed,
}

impl QueueDirectory {
    /// All four, in the order a job passes through them.
    pub const ALL: [QueueDirectory; 4] = [
        QueueDirectory::Queue,
        QueueDirectory::InProgress,
        QueueDirectory::Completed,
        QueueDirectory::Failed,
    ];

    /// The directory's name on disk.
    pub fn on_disk_name(self) -> &'static str {
        match self {
            QueueDirectory::Queue => "_queue",
            QueueDirectory::InProgress => "_in_progress",
            QueueDirectory::Completed => "_completed",
            QueueDirectory::Failed => "_failed",
        }
    }
}

/// Manages the job queue with atomic operations for distributed processing
pub struct JobQueue {
    /// The directory a scan walked. Held so a job's absolute input path can be
    /// cut back to the library-relative form prioritisation reads, and to
    /// resolve the relative paths in legacy job files.
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

        // Load all jobs and take a sort key from each
        let mut jobs_with_keys = Vec::new();
        for job_path in job_files {
            // Try to read the job file
            if let Ok(content) = async_fs::read_to_string(&job_path).await {
                if let Ok(job) = serde_json::from_str::<Job>(&content) {
                    let sort_key = job.episode_sort_key(&self.media_root);
                    jobs_with_keys.push((job_path, job, sort_key));
                }
            }
        }

        // Sort jobs by priority:
        // 1. Episode jobs first (those with a key)
        // 2. Within episodes: series directory, then season, then episode - the
        //    `EpisodeSortKey` ordering, so this is not a second description of it
        // 3. Everything with no key last, in the order it was read
        jobs_with_keys.sort_by(|a, b| match (&a.2, &b.2) {
            (Some(key_a), Some(key_b)) => key_a.cmp(key_b),
            (Some(_), None) => std::cmp::Ordering::Less, // Episode jobs first
            (None, Some(_)) => std::cmp::Ordering::Greater, // Episode jobs first
            (None, None) => std::cmp::Ordering::Equal,   // Maintain order for non-episodes
        });

        // Try to claim jobs in priority order
        for (job_path, _, _) in jobs_with_keys {
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
    ///
    /// A job held under a `.taken` name counts as being worked on. It is one
    /// mid-move, and that is what lets the sweep put an interrupted move back:
    /// while the `.taken` file is there no scan re-enqueues its input, so
    /// nothing else can occupy the name it has to return to.
    pub async fn job_exists(&self, job: &Job) -> Result<bool> {
        let job_filename = job.job_filename();
        let in_progress = self.in_progress_dir.join(&job_filename);
        Ok(self.queue_dir.join(&job_filename).exists()
            || taken_path_for(&in_progress).exists()
            || in_progress.exists()
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
        let mut taken = Vec::new();

        let mut entries = async_fs::read_dir(&self.in_progress_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("job") => stranded_jobs.push(path),
                Some("heartbeat") => heartbeats.push(path),
                Some(TAKEN_EXTENSION) => taken.push(path),
                _ => continue,
            }
        }

        // A sweeper or a worker killed part-way through a move leaves the job
        // under its `.taken` name, where nothing claims it. Put it back, and the
        // pass below treats it as the stranded job it is.
        //
        // Only a `.taken` file that has gone quiet is recovered, because a live
        // one is a move still running and taking it back would leave its mover
        // writing a file it no longer owns. `take_for_move` marks a job before it
        // takes it, so a `.taken` file reads as attended from the moment it
        // exists: quiet here means the mover really is gone, not merely that the
        // job was quiet before somebody picked it up.
        for path in taken {
            if !quiet_for(modified_at(&path).await, stale_after) {
                continue;
            }

            let job_path = path.with_extension("");
            match async_fs::rename(&path, &job_path).await {
                Ok(_) => {
                    warn!("Recovered a job left mid-move: {}", file_name_of(&job_path));
                    outcome.recovered.push(file_name_of(&job_path));
                    stranded_jobs.push(job_path);
                }
                // Another sweeper recovered it first, which is the same
                // rename and so the same single winner.
                Err(e) => debug!("Could not recover {path:?}: {e}"),
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

            let reclaimed = match self.reclaim_one(path, &job_name, stale_after).await {
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
                Reclaimed::Counted { parked: false, .. } => {
                    info!("♻️ Reclaimed abandoned job: {job_name}");
                    outcome.reclaimed.push(job_name);
                }
                Reclaimed::Counted {
                    attempts,
                    parked: true,
                } => {
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
    /// [`Reclaimed::Lost`] means the job is not this sweep's to move, which is
    /// not an error: the loser of a race simply does nothing.
    ///
    /// The caller's staleness check is only a way of not disturbing claims that
    /// are obviously live. It cannot be the decision, because the sweep reads
    /// the whole directory before it moves anything, so by the time a job is
    /// reached the file under its name may be a claim taken since. The decision
    /// is the pair below: take the file, *then* ask whether it is stale, and put
    /// it back if it is not.
    async fn reclaim_one(
        &self,
        in_progress_path: &std::path::Path,
        job_name: &str,
        stale_after: Duration,
    ) -> Result<Reclaimed> {
        // Take the job before reading a byte of it. The count has to be written
        // into the file before it moves on, and a write is not a rename: it
        // creates the file when it is absent and replaces it when it is not, so
        // a sweeper writing to the claim path can resurrect a job somebody else
        // has since claimed and hand one input to two encoders. Renaming to a
        // name of our own picks one winner the way every other move here does,
        // and everything after this point runs on a file nothing else will move.
        let heartbeat_path = heartbeat_path_for(in_progress_path);

        // Read when the job itself was last written before taking it, because
        // taking marks the file and marking overwrites that timestamp. It is
        // only ever a fallback: a job claimed by a worker whose first heartbeat
        // never landed has nothing else to be judged by, and without this it
        // would sit in `_in_progress` for good.
        let job_last_written = modified_at(in_progress_path).await;

        let Some(taken_path) = take_for_move(in_progress_path).await? else {
            return Ok(Reclaimed::Lost);
        };
        let taken_path = taken_path.as_path();

        // Now that the file cannot move, ask again whether its worker is gone.
        // Taking the file says nothing about that: a claim taken a moment ago
        // renames just as willingly as one abandoned an hour ago, and the sweep
        // read this directory before it moved anything, so the file under a name
        // it judged quiet may be a claim taken since.
        //
        // The heartbeat is what tells them apart, and it is read *after* the
        // take. A worker writes one before it is handed its claim, so a fresh
        // heartbeat here is a worker that took this job while the sweep was
        // still reading - and nothing can claim it now, because the job is in no
        // directory a claim looks in.
        let seen = later_of(job_last_written, modified_at(&heartbeat_path).await);
        if !quiet_for(seen, stale_after) {
            // Putting it back cannot land on anything: this sweep holds the only
            // copy, and while it does, no scan re-enqueues the input.
            async_fs::rename(taken_path, in_progress_path).await?;
            return Ok(Reclaimed::Lost);
        }

        let content = async_fs::read_to_string(taken_path).await?;

        // A read that failed is worth another sweep - a network work root drops
        // out from under a worker now and then - but contents that are not a job
        // will not become one, and nothing else ever looks in `_in_progress`.
        let mut job: Job = match serde_json::from_str(&content) {
            Ok(job) => job,
            Err(e) => {
                return self
                    .quarantine_unreadable(taken_path, job_name, &e.to_string())
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
        write_job_atomically(taken_path, &job).await?;

        let destination = if park {
            self.failed_dir.join(job_name)
        } else {
            self.queue_dir.join(job_name)
        };
        if let Some(parent) = destination.parent() {
            async_fs::create_dir_all(parent).await?;
        }

        // Nothing can be at the destination: the job is in no directory a scan
        // consults except under the `.taken` name this holds, so it cannot have
        // been enqueued again while the count was being written.
        async_fs::rename(taken_path, &destination).await?;

        Ok(Reclaimed::Counted {
            attempts,
            parked: park,
        })
    }

    /// Move a job file that cannot be read as a job out of `_in_progress`.
    ///
    /// Contents that are not a job are not going to become one, so a sweep that
    /// only warned about them would warn on every worker startup forever, while
    /// `job_exists` - which goes by filename - kept the media file out of the
    /// queue for just as long. `_failed` is where a person already looks for a
    /// job that needs a decision, and the parse error goes beside it.
    ///
    /// `taken_path` is the job under the name its sweeper took it under, so the
    /// move out is the second half of a move already begun rather than a race
    /// with anything.
    async fn quarantine_unreadable(
        &self,
        taken_path: &std::path::Path,
        job_name: &str,
        reason: &str,
    ) -> Result<Reclaimed> {
        async_fs::create_dir_all(&self.failed_dir).await?;
        let destination = self.failed_dir.join(job_name);

        // A rename, like every other move between these directories.
        async_fs::rename(taken_path, &destination).await?;

        let mut note = destination.into_os_string();
        note.push(".error");
        let _ = async_fs::write(PathBuf::from(note), reason.as_bytes()).await;

        Ok(Reclaimed::Unreadable)
    }

    /// The path of one of the four directories a job moves between.
    pub fn path_of(&self, directory: QueueDirectory) -> &std::path::Path {
        match directory {
            QueueDirectory::Queue => &self.queue_dir,
            QueueDirectory::InProgress => &self.in_progress_dir,
            QueueDirectory::Completed => &self.completed_dir,
            QueueDirectory::Failed => &self.failed_dir,
        }
    }

    /// Remove the named queue directories, and nothing else.
    ///
    /// The queue knows how to carry this out; it does not decide whether it is a
    /// good idea. *Which* directories, and whether the user has agreed to lose
    /// what is in them, belongs to [`crate::commands::clean::CleanCommand`],
    /// because the four are not equally reconstructible and only the caller
    /// knows which ones were asked for.
    ///
    /// A directory that is not there is not an error: a work root nothing has
    /// scanned into is already in the state this leaves one in.
    pub async fn clean(&self, directories: &[QueueDirectory]) -> Result<()> {
        for directory in directories {
            let path = self.path_of(*directory);
            if path.exists() {
                async_fs::remove_dir_all(path).await?;
            }
        }
        Ok(())
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
    /// Jobs found under a `.taken` name - a move interrupted part-way - and put
    /// back where the sweep could finish it. These are reclaimed in the same
    /// pass, so a name here usually appears in `reclaimed` or `parked` too.
    pub recovered: Vec<String>,
}

impl SweepOutcome {
    /// Whether the sweep moved anything at all.
    pub fn is_empty(&self) -> bool {
        self.reclaimed.is_empty()
            && self.parked.is_empty()
            && self.unreadable.is_empty()
            && self.recovered.is_empty()
    }
}

/// What the sweep did with one file in `_in_progress`.
enum Reclaimed {
    /// The attempt was counted and the job moved on: to `_failed` when `parked`,
    /// otherwise back to `_queue`.
    ///
    /// Its own shape rather than a [`FailureDisposition`], because a sweep that
    /// did not take the job reports [`Reclaimed::Lost`] before it has an attempt
    /// to count - so a counted-but-lost reclaim is not a state that exists.
    Counted { attempts: u32, parked: bool },
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

/// The extension a job carries while it is being moved out of `_in_progress`.
const TAKEN_EXTENSION: &str = "taken";

/// The name a mover holds a job under while it moves it out of `_in_progress`.
///
/// Moving a job means writing the attempt count into it and then renaming it on,
/// and a write is not a rename: it creates the file when it is absent and
/// replaces it when it is not. Doing that to the claim path lets a mover working
/// from an out-of-date read put a job that somebody else has since claimed back
/// into the queue, which is how one input reaches two encoders. Renaming to this
/// name first makes the move begin with the same primitive every other move
/// here uses, so exactly one mover proceeds and it owns the file it writes.
///
/// The name is derived, not random, so a `.taken` file left by a mover that died
/// is replaced by the next one to take that job rather than accumulating.
pub(crate) fn taken_path_for(in_progress_path: &std::path::Path) -> PathBuf {
    let mut name = in_progress_path.as_os_str().to_os_string();
    name.push(".");
    name.push(TAKEN_EXTENSION);
    PathBuf::from(name)
}

/// The name of a file, for a log line or a report.
fn file_name_of(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The heartbeat file that sits beside a claimed job.
///
/// It is a separate file rather than a touch of the job file itself so that a
/// worker killed mid-heartbeat cannot leave a half-written job behind.
pub(crate) fn heartbeat_path_for(in_progress_path: &std::path::Path) -> PathBuf {
    let mut name = in_progress_path.as_os_str().to_os_string();
    name.push(".heartbeat");
    PathBuf::from(name)
}

/// When a claimed job last showed a sign of life.
///
/// The most recent of the two timestamps wins, so a job claimed by a worker that
/// has not yet written its first heartbeat is judged by its own mtime. `None`
/// means neither timestamp could be read.
///
/// This is the single definition of "how long has this job been quiet", shared
/// by the sweep below and by the status command. They must agree: a report that
/// called a job stranded on a different rule from the one that reclaims it would
/// be worse than no report.
pub(crate) async fn last_activity(
    job_path: &std::path::Path,
    heartbeat_path: &std::path::Path,
) -> Option<SystemTime> {
    later_of(
        modified_at(job_path).await,
        modified_at(heartbeat_path).await,
    )
}

/// When a file was last written, or `None` if that cannot be read.
pub(crate) async fn modified_at(path: &std::path::Path) -> Option<SystemTime> {
    async_fs::metadata(path).await.ok()?.modified().ok()
}

/// The later of two timestamps, treating an unreadable one as no evidence.
fn later_of(a: Option<SystemTime>, b: Option<SystemTime>) -> Option<SystemTime> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (found, None) | (None, found) => found,
    }
}

/// Whether a sign of life is old enough to call its job abandoned.
///
/// No timestamp at all is treated as not quiet: refusing to reclaim is always
/// the safe answer.
fn quiet_for(seen: Option<SystemTime>, stale_after: Duration) -> bool {
    match seen.and_then(|t| SystemTime::now().duration_since(t).ok()) {
        Some(age) => age >= stale_after,
        None => false,
    }
}

/// Whether a failed filesystem operation means "there is nothing at that path"
/// or "we could not get to it".
///
/// This is the single definition of that distinction, and every place in the
/// queue that treats a missing file as an ordinary outcome has to use it. The
/// three of them ask it for the same reason: losing a race to another worker is
/// routine and is reported as a disposition, while a work root that has dropped
/// out is an error, and answering the second with the first hides an outage
/// behind a shrug. CLAUDE.md designs for many workers on one shared work root,
/// so that is not a hypothetical.
///
/// It cannot rest on the error kind alone. Off Windows it can: a mount that has
/// gone away reports a connection or IO failure, and never `NotFound`.
///
/// Windows folds the entire network-error family into `NotFound`. A work root on
/// a host that is not answering fails with `ERROR_BAD_NETPATH`, which
/// `io::Error` reports as `ErrorKind::NotFound` with raw code 53, and a share
/// that does not resolve gives `ERROR_BAD_NET_NAME` as `NotFound` with 67 - both
/// indistinguishable by kind from a path that was looked up and found missing.
/// So on Windows the raw code is the only evidence there is, and these are the
/// codes that mean the path was never resolved.
///
/// The list is what is known to arrive this way, not a proof that nothing else
/// does, so a caller may say "this is absent" and must not go on to say "and
/// therefore that other thing is what is missing".
///
/// This is the same split as `FFmpegProcessor::ffmpeg_command`: both worker
/// platforms matter, and neither branch is the general case.
#[cfg(windows)]
pub(crate) fn is_absent(error: &std::io::Error) -> bool {
    /// Windows errors that arrive as `NotFound` but mean the path could not be
    /// resolved over the network, not that nothing is there.
    const UNREACHABLE: &[i32] = &[
        51,   // ERROR_REM_NOT_LIST: the remote computer is not available
        53,   // ERROR_BAD_NETPATH: the network path was not found
        54,   // ERROR_NETWORK_BUSY
        55,   // ERROR_DEV_NOT_EXIST: the network resource is no longer available
        64,   // ERROR_NETNAME_DELETED: the share went away
        67,   // ERROR_BAD_NET_NAME: the network name cannot be found
        1231, // ERROR_NETWORK_UNREACHABLE
        1232, // ERROR_HOST_UNREACHABLE
    ];

    error.kind() == std::io::ErrorKind::NotFound
        && !error
            .raw_os_error()
            .is_some_and(|code| UNREACHABLE.contains(&code))
}

#[cfg(not(windows))]
pub(crate) fn is_absent(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// Mark a job as being attended to, without reading or replacing it.
///
/// Opening for write cannot create the file, so this fails rather than
/// resurrecting a job that has moved on. It records nothing except that somebody
/// was here, which is the one thing a timestamp can say honestly.
///
/// `Ok(false)` means the job is not there, which is the ordinary way to lose a
/// race. Anything else is an error, and stays one: on a network work root a
/// refused open is a work root that has dropped out, and reporting that as
/// another worker having been quicker would hide it. That is what [`is_absent`]
/// is for, and why the test cannot be `ErrorKind::NotFound` - which is also the
/// kind Windows gives a share that is not answering, so the kind alone says the
/// job is gone on exactly the setup this function's caution is written for.
async fn mark_attended(path: &std::path::Path) -> Result<bool> {
    let path = path.to_path_buf();
    let marked = tokio::task::spawn_blocking(move || {
        let file = match std::fs::File::options().write(true).open(&path) {
            Ok(file) => file,
            Err(e) if is_absent(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now()))?;
        Ok(true)
    })
    .await??;

    Ok(marked)
}

/// Take a job out of `_in_progress` so that it can be rewritten and moved on.
///
/// Returns the name it is now held under, or `None` if it is not there to take.
/// A work root that cannot be reached at all is an error rather than a `None`,
/// so that a sweep says so instead of reporting a lost race it never ran.
///
/// The mark comes *before* the take, and the order is the whole point. Renaming
/// preserves the timestamp, so a job taken without one first would sit under its
/// `.taken` name still carrying the quiet timestamp that made it eligible - and
/// a second sweep enumerating `_in_progress` just then would judge the move
/// abandoned and recover the file out from under the mover that is holding it.
/// The mover would then write the file back into existence, and the job would be
/// in `_queue` and `_in_progress` at once, which is the whole thing this avoids.
///
/// Marking first closes that: from the moment a job is takeable it already reads
/// as attended, so the `.taken` file is never quiet while a mover lives. A mover
/// that dies between the two leaves the job looking fresh for `stale_after` and
/// the sweep after that collects it, which costs a delay and nothing else.
async fn take_for_move(in_progress_path: &std::path::Path) -> Result<Option<PathBuf>> {
    if !mark_attended(in_progress_path).await? {
        return Ok(None);
    }

    let taken_path = taken_path_for(in_progress_path);
    match async_fs::rename(in_progress_path, &taken_path).await {
        Ok(_) => Ok(Some(taken_path)),
        // Somebody else took it between the mark and the rename.
        Err(_) => Ok(None),
    }
}

/// Whether a claimed job has gone quiet for longer than `stale_after`.
///
/// A timestamp that cannot be read at all is treated as not stale: refusing to
/// reclaim is always the safe answer.
///
/// `clean` asks the same question for the opposite reason - a job that is *not*
/// stale is one a worker is still running, and deleting its claim would leave
/// that worker encoding into a directory that no longer exists. Both callers
/// must use this one rule: a `clean` that judged liveness on its own threshold
/// would refuse jobs the sweep will reclaim, or delete jobs it will not.
pub(crate) async fn is_stale(
    job_path: &std::path::Path,
    heartbeat_path: &std::path::Path,
    stale_after: Duration,
) -> bool {
    match last_activity(job_path, heartbeat_path)
        .await
        .and_then(|t| SystemTime::now().duration_since(t).ok())
    {
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

/// Where a finished job went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionDisposition {
    /// The job was moved to `_completed`.
    Recorded,
    /// The claim was gone before the success could be recorded: a sweep judged
    /// this worker stranded and gave the job to somebody else. Nothing was
    /// written, because the job is no longer this worker's to describe.
    Lost,
}

/// Where a failed job went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureDisposition {
    /// The job went back to `_queue` and will be tried again.
    Requeued { attempts: u32 },
    /// The job has failed too often and was parked in `_failed`.
    Parked { attempts: u32 },
    /// The claim was gone before the failure could be recorded: a sweep judged
    /// this worker stranded and gave the job to somebody else. Nothing was
    /// written, because the job is no longer this worker's to describe.
    Lost,
}

impl<'a> ClaimedJob<'a> {
    /// Record the job as completed and move it to `_completed`.
    ///
    /// Losing the race to a sweep is a disposition, not an error, for the same
    /// reason it is in [`ClaimedJob::fail`]: a worker that went quiet long
    /// enough - a stalled encode, a suspended machine - may have had its job
    /// swept back to the queue and handed to somebody else while it was still
    /// running. The output is already in the library by then, so there is
    /// nothing to recover and nothing to retry; the only thing left to decide is
    /// what the worker does next, and the answer is take the next job. Reporting
    /// it as an error made the worker log a failure and sleep on a run that
    /// succeeded.
    ///
    /// The rename is what asks the question, so the answer cannot be out of
    /// date: it moves the file or it says there was nothing to move. What it
    /// does not say is *which* path was missing. A `clean` that removed
    /// `_completed` out from under a running worker therefore reads as a lost
    /// claim, and the job file stays in `_in_progress` for the next sweep to
    /// requeue - one re-encode that `output_exists` then short-circuits.
    /// Creating the destination first would narrow that, and is deliberately not
    /// done: it would make a work root somebody is clearing look like one that is
    /// fine, and it cannot make the absence unambiguous anyway, because
    /// [`is_absent`] recognises the network failures it knows about rather than
    /// proving there are no others.
    ///
    /// [`is_absent`] is what decides, and the test cannot be
    /// `ErrorKind::NotFound`: Windows reports an unanswering share as `NotFound`
    /// too, so the kind alone would file a work root that has dropped out as a
    /// job somebody else finished - on the platform where a shared work root is
    /// most likely.
    ///
    /// What it still cannot say is whether the job it moved is *this* worker's.
    /// A sweep's copy claimed by somebody else in between is renamed into
    /// `_completed` just as willingly, because nothing in a job file or beside
    /// it identifies which worker holds it. That worker's own `complete` then
    /// reports `Lost` in turn - which, since its output is in the library too,
    /// costs a duplicate encode already spent and nothing else.
    ///
    /// The heartbeat is removed only on the path that moved the job. On the lost
    /// path it belongs to whoever holds the claim now, and deleting it would
    /// leave that worker judged by its job file's mtime alone and swept off a
    /// job it is still running.
    pub async fn complete(mut self) -> Result<CompletionDisposition> {
        self.stop_heartbeat();

        let completed_path = self.queue.completed_dir.join(&self.job_name);

        match async_fs::rename(&self.in_progress_path, &completed_path).await {
            Ok(()) => {
                let _ = async_fs::remove_file(heartbeat_path_for(&self.in_progress_path)).await;
                debug!("Marked job as completed: {}", self.job_name);
                Ok(CompletionDisposition::Recorded)
            }
            Err(e) if is_absent(&e) => Ok(CompletionDisposition::Lost),
            // A work root that has dropped out is not a lost race, and saying so
            // would hide it.
            Err(e) => Err(anyhow!(
                "Could not record {} as completed: {e}",
                self.job_name
            )),
        }
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

        // Take the job before recording anything against it. A worker that went
        // quiet long enough - a stalled encode, a suspended machine - may have
        // had its job swept back to the queue, and a write to the claim path
        // creates the file when it is absent: the failure would put a claim back
        // in `_in_progress` from nothing, for the next sweep to hand to a second
        // worker. The rename cannot create anything, so it says whether the job
        // is still here to be recorded against.
        //
        // It does not say whether the job here is still *this* worker's. If the
        // sweep's copy has since been claimed by somebody else, this rename
        // takes that worker's claim, because nothing in a job file or beside it
        // identifies which worker holds it. Telling those apart needs a claim to
        // carry an identity, which is a larger change than this one.
        let Some(taken_path) = take_for_move(&self.in_progress_path).await? else {
            return Ok(FailureDisposition::Lost);
        };

        self.job.attempts += 1;
        // Keep the tail of the message: that is where FFmpeg says what actually
        // went wrong, and a job file is no place for a megabyte of log.
        self.job.last_error = Some(tail(error, 2000));

        let attempts = self.job.attempts;
        let park = attempts >= MAX_ATTEMPTS;

        // Safe now: the job is held under a name nothing else looks for, so the
        // rewrite cannot land on anybody's claim. It is itself a rename onto
        // that file, so a worker killed here leaves a job the next sweep can
        // still read and put back.
        write_job_atomically(&taken_path, &self.job).await?;

        let destination = if park {
            self.queue.failed_dir.join(&self.job_name)
        } else {
            self.queue.queue_dir.join(&self.job_name)
        };

        if let Some(parent) = destination.parent() {
            async_fs::create_dir_all(parent).await?;
        }
        async_fs::rename(&taken_path, &destination).await?;
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
    use crate::job::{Job, MediaFileType, Operation, PostProcessingSettings, QualitySettings};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::test;

    /// A library-relative episode path, assembled the way a scan assembles one.
    ///
    /// Component-wise rather than as a `/` literal, so the separators are the
    /// platform's own - which is what `WalkDir` and `strip_prefix` hand to
    /// `Job::new`, and the only shape that exercises a path a worker really sees.
    fn episode_path(root: &str, series: &str, season: &str, file: &str) -> PathBuf {
        PathBuf::from(root).join(series).join(season).join(file)
    }

    /// Build a queue with one job already claimed, ready to be aged.
    ///
    /// Dropping the claim is what an interrupted worker does: the job file
    /// stays in `_in_progress` and the heartbeat stops being refreshed.
    async fn queue_with_one_claimed_job(temp_dir: &TempDir) -> (JobQueue, String) {
        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue_with_one_claimed_job_for(temp_dir, &job).await
    }

    /// As above, for a job the caller has built itself.
    async fn queue_with_one_claimed_job_for(temp_dir: &TempDir, job: &Job) -> (JobQueue, String) {
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        queue.enqueue_job(job).await.unwrap();

        let job_name = {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed.job_name().to_string()
        };

        (queue, job_name)
    }

    /// A job whose file is too big to be written in one go, so that a reader
    /// running alongside a writer lands inside the write rather than between
    /// two of them. Only the length of the path matters; nothing opens it.
    fn a_job_whose_file_takes_a_while_to_write(media_root: &Path) -> Job {
        Job::new(
            PathBuf::from(format!("{}.mkv", "l".repeat(64 * 1024))),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            media_root,
        )
    }

    /// Watches a job file from a thread of its own and reports every moment it
    /// held less than a whole job.
    ///
    /// This is the only way to observe the difference a staged write makes.
    /// Writing straight to the job path truncates it first, so for as long as
    /// the write runs the path holds a file that is there and is not a job -
    /// which is what the next reader of that path gets. Writing to a staging
    /// name and renaming means the path only ever holds one whole version or
    /// the other, and this thread finds nothing.
    ///
    /// It samples the length rather than parsing the contents because a sample
    /// has to be cheap to be dense: a read of the whole file takes longer than
    /// the write it is trying to catch.
    struct Reader {
        stop: Arc<AtomicBool>,
        thread: std::thread::JoinHandle<Vec<u64>>,
    }

    impl Reader {
        /// Watch `path`, where a whole job is at least `whole` bytes long.
        fn watching(path: PathBuf, whole: u64) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let until = Arc::clone(&stop);

            let thread = std::thread::spawn(move || {
                let mut torn = Vec::new();
                while !until.load(Ordering::Relaxed) {
                    // A file that is momentarily absent, or that the OS will
                    // not hand over while it is being replaced, is not an
                    // observation: the question is only what a reader that does
                    // get an answer is told.
                    // Through an open handle, because the length in a
                    // directory entry lags behind the file itself.
                    if let Ok(file) = std::fs::File::open(&path) {
                        if let Ok(metadata) = file.metadata() {
                            if metadata.len() < whole {
                                torn.push(metadata.len());
                            }
                        }
                    }
                }
                torn
            });

            Self { stop, thread }
        }

        /// Stop watching, and give back the length of every partial job file
        /// that was on the path while it ran.
        fn stop(self) -> Vec<u64> {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.join().unwrap()
        }
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

    /// The sweep reads the whole of `_in_progress` before it moves anything, so
    /// a job it judged stranded can be a fresh claim by the time it is reached:
    /// another sweeper put the job back, and a worker took it. Deciding on the
    /// judgement made earlier hands that worker's input to a second encoder.
    ///
    /// Driven straight at `reclaim_one`, because that gap is the whole subject.
    /// Going through the sweep would test the cheap check at the top of the loop
    /// instead, which is not what decides this.
    #[test]
    async fn a_job_claimed_since_the_sweep_looked_is_left_with_its_worker() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        // What the sweep sees when it gets there: a job file that has been
        // sitting since it was queued, and a heartbeat written moments ago by
        // the worker that has just claimed it.
        let in_progress_path = queue.in_progress_dir.join(&job_name);
        age(&[&in_progress_path], Duration::from_secs(600));

        let reclaimed = queue
            .reclaim_one(&in_progress_path, &job_name, Duration::from_secs(300))
            .await
            .unwrap();

        assert!(
            matches!(reclaimed, Reclaimed::Lost),
            "a job whose worker is beating its heart is not this sweep's to take"
        );
        assert!(
            in_progress_path.exists(),
            "the claim is put back under the name its worker holds"
        );
        assert!(
            heartbeat_of(&in_progress_path).exists(),
            "and its heartbeat is left alone, or the next sweep takes it"
        );
        assert!(
            !queue.queue_dir.join(&job_name).exists(),
            "nothing is put back in the queue for a second worker to claim"
        );
        assert!(
            !taken_path_for(&in_progress_path).exists(),
            "and nothing is left behind under the name the sweep took it under"
        );
    }

    /// A worker can go quiet long enough to be swept - a stalled encode, a
    /// suspended machine - and only find out when its FFmpeg finally returns.
    /// Writing the failure to the claim path would then put the job back in
    /// `_in_progress` from nothing, where the next sweep hands it to a second
    /// worker while this one is still holding a work folder for it.
    #[test]
    async fn a_worker_whose_job_was_swept_away_does_not_put_it_back() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();
        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        let job_name = claimed.job_name().to_string();
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // A sweep judged this worker gone and put its job back in the queue.
        let queued_path = queue.queue_dir.join(&job_name);
        std::fs::rename(&in_progress_path, &queued_path).unwrap();

        let disposition = claimed.fail("ffmpeg gave up").await.unwrap();

        assert_eq!(
            disposition,
            FailureDisposition::Lost,
            "the job was not this worker's to record against"
        );
        assert!(
            !in_progress_path.exists(),
            "and no claim is conjured back onto the path the sweep emptied"
        );
        assert!(
            !taken_path_for(&in_progress_path).exists(),
            "nor left behind mid-move"
        );
        assert!(queued_path.exists(), "the sweep's copy is the only one");
    }

    /// The same race on the other side: the worker was late rather than dead,
    /// the encode succeeded, and by the time it goes to record that, a sweep has
    /// put the job back in the queue.
    ///
    /// The output is already in the library, so there is nothing here for the
    /// worker to do about it and nothing for it to stop for. It says so and
    /// carries on, and it leaves the heartbeat alone: that file speaks for
    /// whoever holds the claim now, and removing it would leave the next worker
    /// judged by its job file's mtime alone.
    #[test]
    async fn a_worker_whose_job_was_swept_away_reports_it_rather_than_failing() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();
        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        let job_name = claimed.job_name().to_string();
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // A sweep judged this worker gone and put its job back in the queue.
        let queued_path = queue.queue_dir.join(&job_name);
        std::fs::rename(&in_progress_path, &queued_path).unwrap();

        let disposition = claimed.complete().await.unwrap();

        assert_eq!(
            disposition,
            CompletionDisposition::Lost,
            "the job was not this worker's to record against"
        );
        assert!(
            !queue.completed_dir.join(&job_name).exists(),
            "and nothing was conjured into _completed from a claim that had gone"
        );
        assert!(queued_path.exists(), "the sweep's copy is the only one");
        assert!(
            heartbeat_of(&in_progress_path).exists(),
            "the heartbeat belongs to whoever claims the job next, not to this worker"
        );
    }

    /// A work root that has dropped out is not a job somebody else finished.
    ///
    /// Windows reports an unreachable share as `ErrorKind::NotFound`, so a
    /// `complete` testing the kind alone files a transient outage as `Lost` -
    /// the worker shrugs, moves on, and the encode it just finished is recorded
    /// nowhere. This is the same mapping `status` already had to defend against,
    /// asserted here against the real filesystem rather than a constructed
    /// error, so it fails if the rename ever stops going through [`is_absent`].
    ///
    /// Serialised with the other test that probes an unresolvable UNC host:
    /// two threads asking the SMB client for a host that is not there at the
    /// same time get a different failure from either asking alone, and the raw
    /// code is the whole subject here.
    #[cfg(windows)]
    #[serial_test::serial(unc_probe)]
    #[test]
    async fn an_unreachable_work_root_is_an_error_and_not_a_lost_claim() {
        let temp_dir = TempDir::new().unwrap();
        let unreachable = PathBuf::from(r"\\no-such-host-xyz\plexify-queue");
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), unreachable);

        // A claim held on a work root that has since stopped answering. Built
        // here rather than claimed, because nothing can be claimed on a share
        // that is not there - which is the point.
        let held_on_a_share_that_is_down = || {
            let job = Job::new(
                PathBuf::from("show.mkv"),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                QualitySettings::default(),
                PostProcessingSettings::default(),
                temp_dir.path(),
            );
            let job_name = job.job_filename();
            ClaimedJob {
                queue: &queue,
                in_progress_path: queue.in_progress_dir.join(&job_name),
                job_name,
                job,
                heartbeat: None,
            }
        };

        let completed = held_on_a_share_that_is_down().complete().await;
        assert!(
            completed.is_err(),
            "an unreachable work root must be reported, not read as a claim \
             another worker took: {completed:?}"
        );

        // The same trap sits under `fail`, in the open `take_for_move` marks
        // with, and it is the same answer there.
        let failed = held_on_a_share_that_is_down().fail("ffmpeg gave up").await;
        assert!(
            failed.is_err(),
            "nor may a failure be filed as a claim another worker took: {failed:?}"
        );
    }

    /// A mover killed between taking a job and putting it down leaves it under
    /// the `.taken` name, where nothing claims it. The next sweep is what brings
    /// it back, exactly as it does for a job stranded any other way.
    #[test]
    async fn a_job_left_behind_by_an_interrupted_move_is_recovered() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        let taken_path = taken_path_for(&in_progress_path);
        std::fs::rename(&in_progress_path, &taken_path).unwrap();
        std::fs::remove_file(heartbeat_of(&in_progress_path)).unwrap();
        age(&[&taken_path], Duration::from_secs(600));

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(swept.recovered, vec![job_name.clone()]);
        assert_eq!(swept.reclaimed, vec![job_name.clone()]);
        assert!(!taken_path.exists());
        assert!(queue.queue_dir.join(&job_name).exists());
    }

    /// While a job is held under a `.taken` name it is in none of the three
    /// directories a scan consults, and a scan that re-queued it would put a
    /// second copy of the same input in front of a worker. It is also what makes
    /// the recovery above safe: nothing can occupy the name it returns to.
    #[test]
    async fn a_job_held_mid_move_still_counts_as_queued() {
        let temp_dir = TempDir::new().unwrap();
        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
            QualitySettings::default(),
            PostProcessingSettings::default(),
            temp_dir.path(),
        );
        let (queue, job_name) = queue_with_one_claimed_job_for(&temp_dir, &job).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        std::fs::rename(&in_progress_path, taken_path_for(&in_progress_path)).unwrap();

        assert!(
            queue.job_exists(&job).await.unwrap(),
            "a job mid-move is still a job this library has"
        );
    }

    /// A move that is still running is not an interrupted one. Recovering it
    /// would put the job back while its mover is still writing to it, and the
    /// mover - whose write creates the file again - would then deliver a second
    /// copy to `_queue`.
    ///
    /// The job here is quiet by the only clock the sweep had before it was
    /// taken, which is the whole difficulty: every job a mover takes is one that
    /// looked abandoned, so a `.taken` file that still carried its old timestamp
    /// would be recovered from under every mover that ever ran. Taking it
    /// through `take_for_move` is what makes it read as attended, so this goes
    /// through the real take rather than a hand-rolled rename.
    #[test]
    async fn a_move_still_in_progress_is_not_recovered() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        age(
            &[&in_progress_path, &heartbeat_of(&in_progress_path)],
            Duration::from_secs(600),
        );

        let taken_path = take_for_move(&in_progress_path).await.unwrap().unwrap();

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert!(
            swept.is_empty(),
            "a job in flight is nobody else's to move: {swept:?}"
        );
        assert!(taken_path.exists(), "left for its mover to finish");
        assert!(!queue.queue_dir.join(&job_name).exists());
        assert!(
            !in_progress_path.exists(),
            "and never put back under the name its mover took it from"
        );
    }

    /// The fallback that keeps a job claimed by a worker whose first heartbeat
    /// never landed from sitting in `_in_progress` for good. Marking the job
    /// overwrites its own timestamp, so the sweep has to read that before it
    /// takes the file rather than after.
    #[test]
    async fn a_job_claimed_without_a_heartbeat_is_still_reclaimed() {
        let temp_dir = TempDir::new().unwrap();
        let (queue, job_name) = queue_with_one_claimed_job(&temp_dir).await;

        let in_progress_path = queue.in_progress_dir.join(&job_name);
        std::fs::remove_file(heartbeat_of(&in_progress_path)).unwrap();
        age(&[&in_progress_path], Duration::from_secs(600));

        let swept = queue
            .reclaim_stranded_jobs(Duration::from_secs(300))
            .await
            .unwrap();

        assert_eq!(swept.reclaimed, vec![job_name.clone()]);
        assert!(queue.queue_dir.join(&job_name).exists());
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
        // It was queued when every job re-encoded, so that is what it still is.
        assert_eq!(job.operation, Operation::Reencode { channels: None });
    }

    #[test]
    async fn a_lock_left_by_an_interrupted_scan_does_not_keep_a_file_out_of_the_queue() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = Job::new(
            PathBuf::from("show.mkv"),
            MediaFileType::Mkv,
            Operation::Reencode { channels: None },
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
    async fn two_scans_of_one_library_never_leave_a_reader_half_a_job() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = a_job_whose_file_takes_a_while_to_write(temp_dir.path());
        let job_path = queue.queue_dir.join(job.job_filename());

        // The job is on disk before the racing scans start, so a reader that
        // finds nothing there has found a job file mid-rewrite - which is what
        // a worker claiming from this queue would be reading.
        queue.enqueue_job(&job).await.unwrap();
        let whole = std::fs::metadata(&job_path).unwrap().len();
        let reader = Reader::watching(job_path.clone(), whole);

        // Two scanners on one work root, writing the same file at the same
        // time, over a job file that is already there. Repeatedly, because the
        // write is a small part of what a scan does and the question is
        // whether the window exists at all.
        for _ in 0..40 {
            let (first, second) = tokio::join!(queue.enqueue_job(&job), queue.enqueue_job(&job));
            first.unwrap();
            second.unwrap();
        }

        let torn = reader.stop();
        assert!(
            torn.is_empty(),
            "a scan left the job path holding {torn:?} bytes, which is not a job"
        );

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
    async fn recording_an_attempt_never_leaves_a_reader_half_a_job() {
        let temp_dir = TempDir::new().unwrap();
        let job = a_job_whose_file_takes_a_while_to_write(temp_dir.path());
        let (queue, job_name) = queue_with_one_claimed_job_for(&temp_dir, &job).await;
        let in_progress_path = queue.in_progress_dir.join(&job_name);

        // Debris under a staging name from a sweeper that was killed part way
        // through a rewrite of its own. The sweep judges jobs, and this is not
        // one: it must be left where it is and counted as nothing.
        let mut debris = in_progress_path.as_os_str().to_os_string();
        debris.push(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let debris = PathBuf::from(debris);
        std::fs::write(&debris, b"{\"id\": \"half a job").unwrap();

        // The sweep records an attempt against the job before it moves it, and
        // that rewrite is the moment a reader can catch it. Nothing but the
        // sweep looks in `_in_progress`, so the reader here stands for the
        // second sweeper: two workers starting together is the ordinary case.
        // A rewrite only ever adds to a job - an attempt count, an error - so
        // anything shorter than the job as claimed is a write in flight.
        //
        // It watches the name the sweep rewrites the job *under*, which is the
        // `.taken` one, not the name the job was claimed under. Watching the
        // claim path would leave this test asserting nothing: the rewrite has
        // not happened there since a mover started taking a job before touching
        // its contents.
        let whole = std::fs::metadata(&in_progress_path).unwrap().len();
        let reader = Reader::watching(taken_path_for(&in_progress_path), whole);

        // The rewrite is a small part of what a sweep does, so one sweep is a
        // thin sample of a window that must not exist at all. Each round puts
        // the job back as it was and abandons it again.
        for _ in 0..40 {
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
                "a job whose worker stopped goes back to the queue"
            );
            assert!(
                swept.parked.is_empty() && swept.unreadable.is_empty(),
                "the sweep judges jobs, and a staging file is not one"
            );

            let requeued: Job = serde_json::from_str(
                &std::fs::read_to_string(queue.queue_dir.join(&job_name)).unwrap(),
            )
            .unwrap();
            assert_eq!(requeued.attempts, 1);

            // Put the job back as the last worker found it and abandon it
            // again, so the next round is the same round.
            queue.enqueue_job(&job).await.unwrap();
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            drop(claimed);
        }

        let torn = reader.stop();
        assert!(
            torn.is_empty(),
            "a sweep left the job path holding {torn:?} bytes, which is not a job"
        );

        assert!(
            debris.exists(),
            "a staging file the sweep did not write is not the sweep's to remove"
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
            Operation::Reencode { channels: None },
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
                episode_path(
                    "Series",
                    "Breaking Bad",
                    "Season 01",
                    "Breaking Bad S01E03 Gray Matter.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                episode_path(
                    "Series",
                    "Breaking Bad",
                    "Season 01",
                    "Breaking Bad S01E01 Pilot.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            // Better Call Saul Season 1 (newer series)
            Job::new(
                episode_path(
                    "Series",
                    "Better Call Saul",
                    "Season 01",
                    "Better Call Saul S01E02 Mijo.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                episode_path(
                    "Series",
                    "Better Call Saul",
                    "Season 01",
                    "Better Call Saul S01E01 Uno.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            // Non-episode job (movie)
            Job::new(
                PathBuf::from("Movies")
                    .join("The Matrix (1999)")
                    .join("The Matrix (1999).mkv"),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
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

    /// Issues #138 and #133, asserted where the order is actually produced.
    ///
    /// The queue is given four files that all carry the marker `S01E01` or
    /// `S01E02`, in two pairs that a sort key taken from the *rendered* series
    /// name cannot separate or keep together: two shows called `Breaking Bad`,
    /// and one directory holding a file that names no series at all. Claiming
    /// them must finish each show before starting the other.
    #[test]
    async fn a_reboot_pair_and_a_bare_filename_are_each_worked_through_in_turn() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();
        let media_root = temp_dir.path();

        // Interleaved on the way in, so an order that comes out right cannot
        // have come from the order they were enqueued in.
        let files = [
            ("Breaking Bad (2020)", "Breaking Bad - S01E02 - Cat.mkv"),
            ("Breaking Bad (2008)", "S01E02 - Cats in the Bag.mkv"),
            ("Breaking Bad (2020)", "Breaking Bad - S01E01 - Pilot.mkv"),
            ("Breaking Bad (2008)", "Breaking Bad - S01E01 - Pilot.mkv"),
        ];

        for (series, file) in files {
            queue
                .enqueue_job(&Job::new(
                    episode_path("Series", series, "Season 01", file),
                    MediaFileType::Mkv,
                    Operation::Reencode { channels: None },
                    QualitySettings::default(),
                    PostProcessingSettings::default(),
                    media_root,
                ))
                .await
                .unwrap();
        }

        let mut claimed_order = Vec::new();
        while let Some(claimed) = queue
            .claim_job(Some(crate::JobPriority::Episode))
            .await
            .unwrap()
        {
            claimed_order.push(crate::paths::to_forward_slashes(&claimed.job.input_path));
            claimed.complete().await.unwrap();
        }

        let series_of = |claimed: &String| {
            claimed
                .rsplit('/')
                .nth(2)
                .expect("an episode path has a series directory")
                .to_string()
        };
        let order: Vec<String> = claimed_order.iter().map(series_of).collect();

        assert_eq!(
            order,
            vec![
                "Breaking Bad (2008)",
                "Breaking Bad (2008)",
                "Breaking Bad (2020)",
                "Breaking Bad (2020)",
            ],
            "each show has to be finished before the other is started: {claimed_order:?}"
        );
        assert!(
            claimed_order[0].ends_with("Breaking Bad - S01E01 - Pilot.mkv")
                && claimed_order[1].ends_with("S01E02 - Cats in the Bag.mkv"),
            "and ordered by its markers, whether or not the file names the series: {claimed_order:?}"
        );
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
                episode_path(
                    "Series",
                    "Breaking Bad",
                    "Season 01",
                    "Breaking Bad S01E03 Gray Matter.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
                quality.clone(),
                post_processing.clone(),
                media_root,
            ),
            Job::new(
                episode_path(
                    "Series",
                    "Breaking Bad",
                    "Season 01",
                    "Breaking Bad S01E01 Pilot.mkv",
                ),
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
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
            Operation::Reencode { channels: None },
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
