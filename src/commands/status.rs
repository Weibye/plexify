//! Report what is in the queue, and why anything in it is not moving.
//!
//! The queue is four directories and a job is a JSON file, so the state of the
//! queue is already fully recorded on disk - it has simply never been readable
//! without `ls` and a text editor. This command reads those directories and
//! nothing else. There is no index to keep, and adding one would give the queue
//! a second source of truth to disagree with itself about.
//!
//! **This command never moves, rewrites, or deletes anything.** It is the one
//! place a user can look at a queue without changing it, which is what makes it
//! safe to run against a work root a worker is busy with. It does not even call
//! [`JobQueue::init`]: a work root that has never held a job must read as empty,
//! not be created by the act of asking about it.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::fs as async_fs;

use crate::job::Job;
use crate::paths::to_forward_slashes;
use crate::queue::{heartbeat_path_for, last_activity, JobQueue, MAX_ATTEMPTS, STALE_AFTER};

/// Report the state of a work root's queue.
pub struct StatusCommand {
    work_root: PathBuf,
}

/// Everything the four queue directories say, as data.
///
/// Returned rather than printed so that a caller other than the CLI - the TUI
/// in #121 is the obvious one - reads the same state the text report is
/// rendered from, instead of parsing the text back out again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStatus {
    /// The work root this describes. Printed first in the report, because
    /// `-w/--work-dir` defaults to the current working directory and a user
    /// looking at an unexpectedly empty queue is usually looking at the wrong
    /// one.
    pub work_root: PathBuf,
    /// Whether the work root holds any of the four queue directories.
    ///
    /// `scan`, `add` and `work` all call [`JobQueue::init`], so a work root any
    /// of them has touched has the directories whether or not a job was ever put
    /// in one. Their absence is therefore evidence the counts do not carry, and
    /// it is the only thing that tells an empty queue apart from a `-w` pointing
    /// somewhere nothing has ever run. `clean` removes them again, so a drained
    /// and cleaned work root does read as untouched - the disk keeps no record
    /// that distinguishes those two, and inventing one would mean a second
    /// source of truth for the queue to disagree with itself about.
    pub initialised: bool,
    /// Jobs waiting in `_queue` for a worker.
    pub queued: usize,
    /// Jobs that finished, in `_completed`.
    pub completed: usize,
    /// Jobs claimed by a worker, in `_in_progress`.
    pub in_progress: Vec<InProgressJob>,
    /// Jobs parked in `_failed`, which no worker will pick up again.
    pub failed: Vec<FailedJob>,
    /// Job files that are present but could not be read as jobs.
    pub unreadable: Vec<UnreadableJob>,
}

/// A job some worker has claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InProgressJob {
    /// The job's filename, which is the v5 id of the file it transcodes.
    pub job_name: String,
    /// The file being transcoded, exactly as the job file records it.
    pub input_path: PathBuf,
    /// Failed attempts already counted against this job.
    pub attempts: u32,
    /// How long since the worker last showed a sign of life, or `None` if
    /// neither the job file nor its heartbeat could be timestamped.
    pub since_last_seen: Option<Duration>,
}

impl InProgressJob {
    /// Whether the worker that claimed this job has stopped checking in.
    ///
    /// Judged on [`STALE_AFTER`] and on [`last_activity`], which is what the
    /// sweep uses, so what this reports is exactly what the next worker start
    /// will reclaim. An unreadable timestamp is not stranded, for the same
    /// reason the sweep will not touch one: no evidence is not evidence.
    pub fn is_stranded(&self) -> bool {
        matches!(self.since_last_seen, Some(age) if age >= STALE_AFTER)
    }
}

/// A job that failed often enough to be parked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedJob {
    /// The job's filename. Moving this file back to `_queue` by hand is what
    /// asks for the job to be retried, so it is worth printing.
    pub job_name: String,
    /// The file that could not be transcoded.
    pub input_path: PathBuf,
    /// How many attempts were made before it was parked.
    pub attempts: u32,
    /// What went wrong on the last attempt, as recorded in the job file.
    pub last_error: Option<String>,
}

/// A file in a queue directory that is named like a job but does not parse as
/// one.
///
/// Reported rather than skipped: a job file nothing can read is a job that will
/// never move, and silently leaving it out of the counts is how a stuck queue
/// stays mysterious.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableJob {
    pub job_name: String,
    /// Which queue directory it sits in, named as it appears on disk.
    pub directory: String,
    pub reason: String,
}

impl QueueStatus {
    /// Whether the four directories hold no job of any kind.
    fn holds_nothing(&self) -> bool {
        self.queued == 0
            && self.completed == 0
            && self.in_progress.is_empty()
            && self.failed.is_empty()
            && self.unreadable.is_empty()
    }

    /// Whether this work root holds no trace of a job *and* no queue at all.
    ///
    /// Distinct from "the queue is empty" on both halves. A work root that has
    /// drained still has `_completed` entries, and one that was initialised and
    /// never scanned into still has the directories. Nothing anywhere, not even
    /// a `_queue` to put a job in, is the signature of a work root nothing has
    /// ever run against - almost always a `-w` that does not match the one
    /// `scan` used.
    pub fn is_untouched(&self) -> bool {
        !self.initialised && self.holds_nothing()
    }

    /// Whether the queue exists and has nothing in it.
    ///
    /// The `-w` advice does not apply here: something initialised this work
    /// root, so it is the queue the user meant and it is simply empty. Telling
    /// them to check the flag would send them after a mistake they did not make.
    pub fn is_empty(&self) -> bool {
        self.initialised && self.holds_nothing()
    }

    /// Claimed jobs whose worker has stopped checking in.
    pub fn stranded(&self) -> impl Iterator<Item = &InProgressJob> {
        self.in_progress.iter().filter(|job| job.is_stranded())
    }

    /// Jobs claimed by a worker that is still checking in.
    pub fn running(&self) -> impl Iterator<Item = &InProgressJob> {
        self.in_progress.iter().filter(|job| !job.is_stranded())
    }
}

impl StatusCommand {
    pub fn new(work_root: PathBuf) -> Self {
        Self { work_root }
    }

    /// Read the four queue directories.
    ///
    /// A directory that does not exist reads as empty rather than as an error.
    /// A work root that has never been initialised is an ordinary thing for a
    /// user to ask about - it is the exact situation this command exists to
    /// explain - so refusing to answer would be refusing at the only moment the
    /// answer matters.
    pub async fn execute(&self) -> Result<QueueStatus> {
        // `media_root` is irrelevant here: nothing this command reads resolves a
        // job path against it. Job input paths are reported exactly as the job
        // file records them.
        let queue = JobQueue::new(self.work_root.clone(), self.work_root.clone());

        let mut unreadable = Vec::new();

        let queued_files = job_files(&queue.queue_dir).await?;
        let completed_files = job_files(&queue.completed_dir).await?;
        let in_progress_files = job_files(&queue.in_progress_dir).await?;
        let failed_files = job_files(&queue.failed_dir).await?;

        // Whether the four directories are there at all is evidence the counts
        // do not carry, and it is what separates an empty queue from a work root
        // nobody has ever pointed `scan` at.
        let initialised = queued_files.is_some()
            || completed_files.is_some()
            || in_progress_files.is_some()
            || failed_files.is_some();

        let queued = queued_files.unwrap_or_default().len();
        let completed = completed_files.unwrap_or_default().len();

        let mut in_progress = Vec::new();
        for path in in_progress_files.unwrap_or_default() {
            let job_name = file_name_of(&path);
            match read_job(&path).await {
                Ok(Some(job)) => {
                    let since_last_seen = last_activity(&path, &heartbeat_path_for(&path))
                        .await
                        .and_then(|seen| SystemTime::now().duration_since(seen).ok());

                    in_progress.push(InProgressJob {
                        job_name,
                        input_path: job.input_path,
                        attempts: job.attempts,
                        since_last_seen,
                    });
                }
                // A worker completed it while this report was being built. The
                // counts are a moment stale, which is the right trade; calling
                // it unreadable would not be.
                Ok(None) => {}
                Err(reason) => unreadable.push(UnreadableJob {
                    job_name,
                    directory: "_in_progress".to_string(),
                    reason,
                }),
            }
        }

        let mut failed = Vec::new();
        for path in failed_files.unwrap_or_default() {
            let job_name = file_name_of(&path);
            match read_job(&path).await {
                Ok(Some(job)) => failed.push(FailedJob {
                    job_name,
                    input_path: job.input_path,
                    attempts: job.attempts,
                    last_error: job.last_error,
                }),
                Ok(None) => {}
                Err(reason) => unreadable.push(UnreadableJob {
                    job_name,
                    directory: "_failed".to_string(),
                    reason,
                }),
            }
        }

        // Directory order is whatever the filesystem feels like. Sort so that
        // two runs against an unchanged queue print the same thing.
        in_progress.sort_by(|a, b| a.input_path.cmp(&b.input_path));
        failed.sort_by(|a, b| a.input_path.cmp(&b.input_path));
        unreadable.sort_by(|a, b| (&a.directory, &a.job_name).cmp(&(&b.directory, &b.job_name)));

        Ok(QueueStatus {
            work_root: self.work_root.clone(),
            initialised,
            queued,
            completed,
            in_progress,
            failed,
            unreadable,
        })
    }
}

/// The `.job` files in a queue directory, or `None` if the directory is not
/// there at all.
///
/// Only `.job` files count. `_queue` also holds the `.lock` directories enqueue
/// uses, and `_in_progress` holds `.heartbeat` files and `.chunks` directories;
/// none of those is a job and counting one would inflate the answer.
///
/// "Not there" is the only absence that reads as an empty directory. Every other
/// failure is propagated, because they mean different things to a user and only
/// one of them is a `-w` problem: a directory that does not exist is a work root
/// nothing has scanned into, while a directory that cannot be *reached* - a
/// share that is down, a mount that went away - is a work root whose `-w` may be
/// exactly right. CLAUDE.md designs for many workers on one shared work root, so
/// the second is not a hypothetical, and answering it with "this work root has
/// never held a job" sends a user to change the one thing that is not wrong.
async fn job_files(directory: &Path) -> Result<Option<Vec<PathBuf>>> {
    let mut entries = match async_fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(unreachable_directory(directory, &e)),
    };

    let mut paths = Vec::new();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => return Err(unreachable_directory(directory, &e)),
        };

        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("job") && path.is_file() {
            paths.push(path);
        }
    }

    Ok(Some(paths))
}

/// Whether a failed read means "there is no such directory" or "we could not
/// get to it".
///
/// The distinction is the whole of the difference between an empty report and
/// an error, so it cannot rest on the error kind alone. Off Windows it can: a
/// mount that has gone away reports a connection or IO failure, and never
/// `NotFound`.
///
/// Windows folds the entire network-error family into `NotFound`. A work root on
/// a host that is not answering fails with `ERROR_BAD_NETPATH`, which
/// `io::Error` reports as `ErrorKind::NotFound` with raw code 53, and a share
/// that does not resolve gives `ERROR_BAD_NET_NAME` as `NotFound` with 67 - both
/// indistinguishable by kind from the `_queue` directory of a work root nothing
/// has scanned into yet. So on Windows the raw code is the only evidence there
/// is, and these are the codes that mean the path was never resolved rather than
/// looked up and found missing.
///
/// This is the same split as `FFmpegProcessor::ffmpeg_command`: both worker
/// platforms matter, and neither branch is the general case.
#[cfg(windows)]
fn is_absent(error: &std::io::Error) -> bool {
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
fn is_absent(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

/// Why a queue directory could not be listed, said in full.
///
/// The whole message is one error rather than an `anyhow` context over the
/// `io::Error`, because `main` prints a failure with `{}` and that shows only
/// the outermost context - which would drop the operating system's reason, the
/// one part of this that says whether the host is down or the path is denied.
fn unreachable_directory(directory: &Path, error: &std::io::Error) -> anyhow::Error {
    anyhow!(
        "could not read the queue directory {}: {error}. \
         The work root could not be reached, so this says nothing about whether \
         the queue is empty - -w may well name exactly the right place.",
        for_display(directory)
    )
}

/// Read one job file, describing the failure rather than propagating it.
///
/// `Ok(None)` means the file is no longer there. A job vanishing between the
/// listing and the read is the healthiest thing a queue does: `complete()`
/// renames the file into `_completed`, and on a busy queue that lands inside
/// this window regularly. Filing it under "could not be read" would report
/// corruption on a queue where a worker is simply finishing work, so only a file
/// that is still present and cannot be parsed is worth reporting - which is also
/// what [`job_files`] already does, dropping an entry that vanished before its
/// `is_file()` check.
async fn read_job(path: &Path) -> std::result::Result<Option<Job>, String> {
    let content = match async_fs::read_to_string(path).await {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not be read: {e}")),
    };

    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| format!("is not a readable job file: {e}"))
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// An age, in the largest unit that still says something useful.
///
/// A zero remainder is dropped rather than printed, so a threshold reads as
/// `5m` instead of `5m 0s`.
fn human_duration(age: Duration) -> String {
    let seconds = age.as_secs();

    let (whole, unit, remainder, remainder_unit) = if seconds < 60 {
        return format!("{seconds}s");
    } else if seconds < 3600 {
        (seconds / 60, "m", seconds % 60, "s")
    } else {
        (seconds / 3600, "h", (seconds % 3600) / 60, "m")
    };

    if remainder == 0 {
        format!("{whole}{unit}")
    } else {
        format!("{whole}{unit} {remainder}{remainder_unit}")
    }
}

/// A path as the report shows it.
///
/// Separators are normalised for display only. A job file records the path with
/// whatever separator the platform used when it was written, so a work root
/// printed with `/` above a media path printed with `\` is routine on Windows
/// and reads as two unrelated things. This changes how the path looks, never
/// which file it names.
fn for_display(path: &Path) -> String {
    to_forward_slashes(path)
}

/// The most recent line of a recorded error, which is where the reason is.
///
/// `ClaimedJob::fail` keeps the *tail* of what FFmpeg said, because that is
/// where FFmpeg finally names the problem, so the last line is the one worth
/// putting in a summary. The job file keeps the rest.
fn error_summary(message: &str) -> (String, usize) {
    let lines: Vec<&str> = message
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    match lines.last() {
        Some(last) => (last.to_string(), lines.len().saturating_sub(1)),
        None => ("no reason was recorded".to_string(), 0),
    }
}

/// Render a status as the text the CLI prints.
///
/// A free function over [`QueueStatus`] rather than a method that prints, so it
/// can be asserted on directly.
pub fn render(status: &QueueStatus) -> String {
    let mut out = String::new();

    out.push_str("\n📊 Queue Status\n");
    out.push_str("═══════════════\n");
    out.push_str(&format!(
        "📂 Work root: {}\n\n",
        for_display(&status.work_root)
    ));

    out.push_str(&format!("   Queued:      {}\n", status.queued));
    out.push_str(&format!("   In progress: {}\n", status.in_progress.len()));
    out.push_str(&format!("   Completed:   {}\n", status.completed));
    out.push_str(&format!("   Failed:      {}\n", status.failed.len()));
    if !status.unreadable.is_empty() {
        out.push_str(&format!("   Unreadable:  {}\n", status.unreadable.len()));
    }

    let stranded: Vec<&InProgressJob> = status.stranded().collect();
    let running: Vec<&InProgressJob> = status.running().collect();

    if !running.is_empty() {
        out.push_str("\n🏃 Being worked on:\n");
        out.push_str("───────────────────\n");
        for job in running {
            out.push_str(&format!("\n  {}\n", for_display(&job.input_path)));
            match job.since_last_seen {
                Some(age) => out.push_str(&format!(
                    "  a worker checked in {} ago\n",
                    human_duration(age)
                )),
                None => out.push_str("  age unknown: its timestamps could not be read\n"),
            }
        }
    }

    if !stranded.is_empty() {
        out.push_str("\n⚠️  Stranded - claimed, but no worker is checking in:\n");
        out.push_str("────────────────────────────────────────────────────\n");
        for job in stranded {
            out.push_str(&format!("\n  {}\n", for_display(&job.input_path)));
            if let Some(age) = job.since_last_seen {
                out.push_str(&format!(
                    "  last seen {} ago, over the {} it is given\n",
                    human_duration(age),
                    human_duration(STALE_AFTER)
                ));
            }
        }
        out.push_str("\n  These are returned to the queue by the next worker that starts.\n");
    }

    if !status.failed.is_empty() {
        out.push_str("\n🚫 Parked - these will not be tried again:\n");
        out.push_str("─────────────────────────────────────────\n");
        for job in &status.failed {
            out.push_str(&format!("\n  {}\n", for_display(&job.input_path)));
            out.push_str(&format!(
                "  failed {} of {} attempts\n",
                job.attempts, MAX_ATTEMPTS
            ));

            match job.last_error.as_deref() {
                Some(message) => {
                    let (summary, omitted) = error_summary(message);
                    out.push_str(&format!("  {summary}\n"));
                    if omitted > 0 {
                        out.push_str(&format!(
                            "  ({omitted} earlier lines are in {})\n",
                            job.job_name
                        ));
                    }
                }
                None => out.push_str("  no reason was recorded\n"),
            }
        }
        out.push_str("\n  Move a job file out of _failed to ask for it to be tried again.\n");
    }

    if !status.unreadable.is_empty() {
        out.push_str("\n❓ Job files that could not be read:\n");
        out.push_str("───────────────────────────────────\n");
        for job in &status.unreadable {
            out.push_str(&format!("\n  {}/{}\n", job.directory, job.job_name));
            out.push_str(&format!("  {}\n", job.reason));
        }
    }

    if status.is_untouched() {
        out.push_str("\n💡 This work root has never held a job.\n");
        out.push_str("   -w/--work-dir defaults to the current working directory, not the media\n");
        out.push_str("   directory, so a queue created from a different shell lives elsewhere.\n");
        out.push_str("   Pass -w to name the queue you mean.\n");
    } else if status.is_empty() {
        out.push_str("\n💡 This queue is empty.\n");
        out.push_str("   Its directories are here, so this is the work root something ran\n");
        out.push_str("   against - there is just nothing waiting, running or parked in it.\n");
        out.push_str("   `scan` is what puts jobs here.\n");
    }

    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{MediaFileType, PostProcessingSettings, QualitySettings};
    use crate::queue::{JobQueue, HEARTBEAT_INTERVAL};
    use tempfile::TempDir;

    fn a_job(input: &str, media_root: &Path) -> Job {
        Job::new(
            PathBuf::from(input),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            media_root,
        )
    }

    /// Backdate a file, so a job looks as though nothing has touched it.
    ///
    /// Unwrapped throughout: a helper that quietly did nothing would leave the
    /// stranded test asserting that a *fresh* job is not stranded, which it
    /// would pass without testing anything.
    fn age(paths: &[PathBuf], by: Duration) {
        let when = std::fs::FileTimes::new().set_modified(SystemTime::now() - by);
        for path in paths {
            let file = std::fs::File::options().write(true).open(path).unwrap();
            file.set_times(when).unwrap();
        }
    }

    /// Claim one job and backdate it, which is exactly what a worker that
    /// stopped checking in `by` ago leaves behind: the job file sits in
    /// `_in_progress` and neither it nor its heartbeat has moved since.
    async fn claim_and_age(queue: &JobQueue, media_root: &Path, input: &str, by: Duration) {
        queue.enqueue_job(&a_job(input, media_root)).await.unwrap();

        let job_name = {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed.job_name().to_string()
        };

        let in_progress = queue.in_progress_dir.join(&job_name);
        let heartbeat = heartbeat_path_for(&in_progress);
        age(&[in_progress, heartbeat], by);
    }

    /// The names of the files a status reports under one of its two verdicts.
    fn inputs<'a>(jobs: impl Iterator<Item = &'a InProgressJob>) -> Vec<String> {
        jobs.map(|job| {
            job.input_path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
    }

    /// A work root nothing has ever scanned into is the case a user hits when
    /// `-w` does not match the one `scan` used, so it has to answer rather than
    /// fail - and it must not create the directories just by being asked.
    #[tokio::test]
    async fn a_work_root_that_has_never_held_a_job_reads_as_empty() {
        let temp_dir = TempDir::new().unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.queued, 0);
        assert_eq!(status.completed, 0);
        assert!(status.in_progress.is_empty());
        assert!(status.failed.is_empty());
        assert!(!status.initialised);
        assert!(status.is_untouched());
        assert!(!status.is_empty());

        assert!(
            !temp_dir.path().join("_queue").exists(),
            "asking about a queue must not create one"
        );

        let report = render(&status);
        assert!(report.contains("never held a job"));
        assert!(report.contains("-w/--work-dir"));
        assert!(!report.contains("This queue is empty"), "{report}");
    }

    /// An initialised but drained queue is *not* the untouched case, and saying
    /// so would send a user chasing a `-w` problem that is not there.
    #[tokio::test]
    async fn an_empty_queue_that_has_run_is_not_reported_as_untouched() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = a_job("done.mkv", temp_dir.path());
        queue.enqueue_job(&job).await.unwrap();
        queue
            .claim_job(None)
            .await
            .unwrap()
            .unwrap()
            .complete()
            .await
            .unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.queued, 0);
        assert_eq!(status.completed, 1);
        assert!(!status.is_untouched());
        assert!(!render(&status).contains("never held a job"));
    }

    #[tokio::test]
    async fn waiting_jobs_are_counted_and_locks_are_not() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        for name in ["one.mkv", "two.mkv", "three.mkv"] {
            queue
                .enqueue_job(&a_job(name, temp_dir.path()))
                .await
                .unwrap();
        }

        // An interrupted enqueue leaves a `.lock` directory behind. It is not a
        // job and must not be counted as one.
        std::fs::create_dir(queue.queue_dir.join("abandoned.job.lock")).unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.queued, 3);
    }

    /// The case the command exists for: a worker was killed, its job is sitting
    /// in `_in_progress`, and nothing today tells a user that.
    #[tokio::test]
    async fn a_stranded_job_is_reported_as_stranded() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let job = a_job(
            "Series/Elementary/Season 01/Elementary - S01E01.mkv",
            temp_dir.path(),
        );
        queue.enqueue_job(&job).await.unwrap();

        // Claiming and dropping the claim is exactly what an interrupted worker
        // leaves behind: the job file stays put and the heartbeat stops.
        let job_name = {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed.job_name().to_string()
        };

        let in_progress = queue.in_progress_dir.join(&job_name);
        let heartbeat = heartbeat_path_for(&in_progress);
        age(
            &[in_progress, heartbeat],
            STALE_AFTER + Duration::from_secs(600),
        );

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.in_progress.len(), 1);
        assert!(status.in_progress[0].is_stranded());
        assert_eq!(status.stranded().count(), 1);
        assert_eq!(status.running().count(), 0);

        let report = render(&status);
        assert!(report.contains("Stranded"), "{report}");
        assert!(report.contains("Elementary - S01E01.mkv"), "{report}");
        assert!(
            report.contains("returned to the queue by the next worker"),
            "{report}"
        );
    }

    /// The threshold has to be the sweep's, not merely near it.
    ///
    /// The stranded test above ages a job three times past `STALE_AFTER`, which
    /// passes under any threshold up to the age it happened to pick, so on its
    /// own it pins nothing. These two sit either side of `STALE_AFTER` itself:
    /// raise the rule and the job just past the line stops being reported,
    /// lower it and the job still well inside the line starts being reported.
    /// A report naming jobs the next worker start will *not* reclaim - or
    /// staying quiet about ones it will - is the failure this command exists to
    /// prevent, so the number is worth pinning rather than approximating.
    #[tokio::test]
    async fn stranded_is_judged_on_the_threshold_the_sweep_uses() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        // Two seconds past the line, not ten minutes past it.
        claim_and_age(
            &queue,
            temp_dir.path(),
            "just-past.mkv",
            STALE_AFTER + Duration::from_secs(2),
        )
        .await;

        // Quiet for longer than a heartbeat interval, which is the nearest
        // other duration in the queue, and still nowhere near stranded.
        claim_and_age(
            &queue,
            temp_dir.path(),
            "well-inside.mkv",
            HEARTBEAT_INTERVAL + Duration::from_secs(5),
        )
        .await;

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.in_progress.len(), 2);
        assert_eq!(
            inputs(status.stranded()),
            vec!["just-past.mkv"],
            "a job quiet for longer than STALE_AFTER is what the sweep reclaims"
        );
        assert_eq!(
            inputs(status.running()),
            vec!["well-inside.mkv"],
            "a job quiet for less than STALE_AFTER is one the sweep leaves alone"
        );
    }

    /// The other half of the same question. A job a worker is genuinely running
    /// looks identical on disk apart from the heartbeat, so calling this one
    /// stranded would make the report useless.
    #[tokio::test]
    async fn a_job_a_worker_is_running_is_not_reported_as_stranded() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        queue
            .enqueue_job(&a_job("running.mkv", temp_dir.path()))
            .await
            .unwrap();
        let claimed = queue.claim_job(None).await.unwrap().unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.in_progress.len(), 1);
        assert!(!status.in_progress[0].is_stranded());
        assert_eq!(status.running().count(), 1);

        let report = render(&status);
        assert!(report.contains("Being worked on"), "{report}");
        assert!(!report.contains("Stranded"), "{report}");

        drop(claimed);
    }

    /// A long encode is the tricky one: the job file itself goes stale while the
    /// worker is perfectly alive, and only the heartbeat says so. Reporting it
    /// as stranded would tell a user to go looking for a dead worker that is
    /// busy transcoding.
    #[tokio::test]
    async fn a_long_encode_is_not_stranded_while_its_heartbeat_is_fresh() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        queue
            .enqueue_job(&a_job("long-film.mkv", temp_dir.path()))
            .await
            .unwrap();
        let claimed = queue.claim_job(None).await.unwrap().unwrap();
        let in_progress = queue.in_progress_dir.join(claimed.job_name());

        // Only the job file is aged; the heartbeat stays fresh.
        age(&[in_progress], STALE_AFTER + Duration::from_secs(6000));

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(
            !status.in_progress[0].is_stranded(),
            "a worker checking in owns its job however long the encode takes"
        );

        drop(claimed);
    }

    /// Surfacing why a job is parked is most of the value: the attempt count and
    /// the message are already in the job file and unreadable without an editor.
    #[tokio::test]
    async fn a_parked_job_reports_its_attempts_and_its_reason() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        queue
            .enqueue_job(&a_job(
                "Movies/Broken (2011)/Broken (2011).mkv",
                temp_dir.path(),
            ))
            .await
            .unwrap();

        for _ in 0..MAX_ATTEMPTS {
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed
                .fail("Opening input\nStream mapping failed\nNo such file or directory")
                .await
                .unwrap();
        }

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(status.failed.len(), 1);
        assert_eq!(status.failed[0].attempts, MAX_ATTEMPTS);
        assert_eq!(status.queued, 0);

        let report = render(&status);
        assert!(report.contains("Parked"), "{report}");
        assert!(report.contains("Broken (2011).mkv"), "{report}");
        assert!(
            report.contains(&format!("failed {MAX_ATTEMPTS} of {MAX_ATTEMPTS} attempts")),
            "{report}"
        );
        // The tail of the message is where the reason is; the job file keeps the
        // rest, and the report says so rather than dropping it silently.
        assert!(report.contains("No such file or directory"), "{report}");
        assert!(report.contains("2 earlier lines"), "{report}");
        assert!(report.contains("out of _failed"), "{report}");
    }

    /// A job file nothing can parse is a job that will never move. Skipping it
    /// would make the counts lie about a queue that is stuck.
    #[tokio::test]
    async fn a_job_file_that_does_not_parse_is_reported_rather_than_skipped() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        std::fs::write(queue.failed_dir.join("garbage.job"), b"not json at all").unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(status.failed.is_empty());
        assert_eq!(status.unreadable.len(), 1);
        assert_eq!(status.unreadable[0].directory, "_failed");
        assert!(!status.is_untouched());

        let report = render(&status);
        assert!(report.contains("could not be read"), "{report}");
        assert!(report.contains("_failed/garbage.job"), "{report}");
    }

    /// A job that finishes while the report is being built is a queue working,
    /// not a queue breaking.
    ///
    /// `job_files` lists `_in_progress`, then each path is read. A worker's
    /// `complete()` renames a job out of that directory in between, and on a
    /// busy queue - the exact moment someone runs this command - it lands inside
    /// that window often. Reporting it under "could not be read" prints a
    /// corruption signal for the healthiest thing the queue does.
    ///
    /// Asserted on `read_job` rather than by racing `execute`, because the
    /// decision this pins is the one this function makes, and a test that has to
    /// win a race to fail is a test that will pass on a slow day.
    #[tokio::test]
    async fn a_job_that_moved_on_is_not_a_job_that_could_not_be_read() {
        let temp_dir = TempDir::new().unwrap();

        // What `complete()` leaves for a reader that listed the file a moment
        // earlier: the path is simply not there any more.
        let moved_on = temp_dir.path().join("finished-mid-read.job");
        assert!(
            matches!(read_job(&moved_on).await, Ok(None)),
            "a job that completed between the listing and the read is not corruption"
        );

        // A file that is still there and does not parse is the real thing, and
        // has to stay reported.
        let corrupt = temp_dir.path().join("corrupt.job");
        std::fs::write(&corrupt, b"not json at all").unwrap();
        assert!(
            read_job(&corrupt).await.is_err(),
            "a job file that is present and unparseable is still unreadable"
        );
    }

    /// A work root that cannot be reached is not an empty one.
    ///
    /// Swallowing every `read_dir` error reports a share that is down as a
    /// queue that has never held a job, and sends the user to change a `-w`
    /// that is already correct - the one mistake this command exists to catch,
    /// made by the command itself.
    #[tokio::test]
    async fn a_queue_directory_that_cannot_be_read_is_not_reported_as_an_empty_queue() {
        let temp_dir = TempDir::new().unwrap();

        // A file where `_queue` should be is a path that exists and cannot be
        // listed: the portable stand-in for the unreachable share, which is the
        // case that actually happens against a shared work root.
        std::fs::write(temp_dir.path().join("_queue"), b"not a directory").unwrap();

        let error = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .expect_err("an unlistable queue directory must not read as an empty queue");

        let message = format!("{error}");
        assert!(message.contains("_queue"), "{message}");
        assert!(message.contains("could not be reached"), "{message}");
        assert!(
            !message.contains("never held a job"),
            "the -w advice belongs to an absent work root, not an unreadable one: {message}"
        );
    }

    /// The unreachable work root that started this, on the platform that hides
    /// it.
    ///
    /// Narrowing to `ErrorKind::NotFound` is the obvious fix and on Windows it
    /// is not enough: `\\host\share` on a host that is not answering comes back
    /// as `NotFound` with raw code 53, so the kind alone still reads an
    /// unreachable share as a work root nothing has scanned into. This asserts
    /// against the real filesystem rather than a constructed error, so it fails
    /// if that mapping is ever what the obvious fix assumed it was.
    #[cfg(windows)]
    #[test]
    fn a_windows_network_path_that_does_not_resolve_is_unreachable_not_absent() {
        let unreachable = std::fs::read_dir(r"\\no-such-host-xyz\plexify-queue\_queue")
            .expect_err("a host that does not exist cannot be listed");

        assert_eq!(
            unreachable.kind(),
            std::io::ErrorKind::NotFound,
            "if Windows ever stops folding network errors into NotFound, the raw \
             code check below is dead weight and can go"
        );
        assert!(
            !is_absent(&unreachable),
            "an unreachable share must not read as an uninitialised work root"
        );

        // A path on a drive that is there, in a directory that is not, is the
        // uninitialised work root - and still has to read as empty.
        let absent = std::fs::read_dir(r"C:\definitely\not\here\_queue")
            .expect_err("a directory that is not there cannot be listed");
        assert!(is_absent(&absent));
    }

    /// An initialised work root with nothing in it is an empty queue, not a `-w`
    /// aimed at the wrong place.
    ///
    /// Both print advice, and it must not be the same advice: something created
    /// these directories, so this *is* the queue the user meant.
    #[tokio::test]
    async fn an_initialised_but_empty_work_root_is_not_reported_as_misaddressed() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(status.initialised);
        assert!(status.is_empty());
        assert!(!status.is_untouched());

        let report = render(&status);
        assert!(report.contains("This queue is empty"), "{report}");
        assert!(!report.contains("never held a job"), "{report}");
        assert!(
            !report.contains("-w/--work-dir"),
            "the -w advice is wrong here - the flag names exactly this queue: {report}"
        );
    }

    /// The work root is the first thing printed, because `-w` defaulting to the
    /// current directory is the most common reason a queue looks wrong.
    #[tokio::test]
    async fn the_report_names_the_work_root_it_is_describing() {
        let temp_dir = TempDir::new().unwrap();

        let status = StatusCommand::new(temp_dir.path().to_path_buf())
            .execute()
            .await
            .unwrap();

        let report = render(&status);
        let work_root_line = report
            .lines()
            .find(|line| line.contains("Work root"))
            .expect("the report must name the work root");

        assert!(work_root_line.contains(&to_forward_slashes(temp_dir.path())));
    }

    /// Two runs against an unchanged queue must print the same thing, or a
    /// report is no use for comparing one moment to the next.
    #[tokio::test]
    async fn jobs_are_reported_in_a_stable_order() {
        let temp_dir = TempDir::new().unwrap();
        let queue = JobQueue::new(temp_dir.path().to_path_buf(), temp_dir.path().to_path_buf());
        queue.init().await.unwrap();

        for name in ["c.mkv", "a.mkv", "b.mkv"] {
            queue
                .enqueue_job(&a_job(name, temp_dir.path()))
                .await
                .unwrap();
            let claimed = queue.claim_job(None).await.unwrap().unwrap();
            claimed.fail("first failure").await.unwrap();
        }

        // Park them all, so `_failed` holds three jobs in filesystem order.
        for _ in 0..(MAX_ATTEMPTS - 1) {
            while let Some(claimed) = queue.claim_job(None).await.unwrap() {
                claimed.fail("later failure").await.unwrap();
            }
        }

        let command = StatusCommand::new(temp_dir.path().to_path_buf());
        let status = command.execute().await.unwrap();

        assert_eq!(status.failed.len(), 3);
        let names: Vec<String> = status
            .failed
            .iter()
            .map(|job| {
                job.input_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.mkv", "b.mkv", "c.mkv"]);

        assert_eq!(render(&status), render(&command.execute().await.unwrap()));
    }

    #[test]
    fn an_age_is_rendered_in_the_largest_useful_unit() {
        assert_eq!(human_duration(Duration::from_secs(9)), "9s");
        assert_eq!(human_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(human_duration(Duration::from_secs(7_320)), "2h 2m");

        // A whole number of units drops the empty remainder, so the threshold
        // this report quotes reads as `5m` rather than `5m 0s`.
        assert_eq!(human_duration(STALE_AFTER), "5m");
        assert_eq!(human_duration(Duration::from_secs(7_200)), "2h");
    }

    /// Windows job files record a media path with `\`, and the work root above
    /// it prints with `/`. Two separators in one report read as two unrelated
    /// places, so display normalises them.
    #[test]
    fn a_reported_path_uses_one_separator() {
        let status = QueueStatus {
            work_root: PathBuf::from("work"),
            initialised: true,
            queued: 0,
            completed: 0,
            in_progress: Vec::new(),
            failed: vec![FailedJob {
                job_name: "id.job".to_string(),
                input_path: PathBuf::from("media")
                    .join("Movies")
                    .join("Heat (1995).mkv"),
                attempts: MAX_ATTEMPTS,
                last_error: Some("broken".to_string()),
            }],
            unreadable: Vec::new(),
        };

        let report = render(&status);
        assert!(report.contains("media/Movies/Heat (1995).mkv"), "{report}");
        assert!(!report.contains('\\'), "{report}");
    }

    #[test]
    fn an_error_summary_keeps_the_last_line_and_counts_the_rest() {
        let (summary, omitted) = error_summary("first\n\nsecond\nthird");
        assert_eq!(summary, "third");
        assert_eq!(omitted, 2);

        let (summary, omitted) = error_summary("only one");
        assert_eq!(summary, "only one");
        assert_eq!(omitted, 0);
    }
}
