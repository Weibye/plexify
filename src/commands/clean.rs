//! Empty a work root's queue directories, after saying what that costs.
//!
//! `clean` is the only command that deletes queue state, and the four
//! directories it empties are not equally disposable:
//!
//! - `_queue` is a list of files to transcode. Running `scan` again rebuilds it.
//! - `_completed` is the record that a file *was* transcoded. Nothing
//!   reconstructs it, and #135 proposes reading it as the authoritative answer
//!   to "did plexify finish this?".
//! - `_failed` holds every parked job's attempt count and recorded error, and is
//!   also what stops the next `scan` re-queueing a job that cannot succeed.
//!   Emptying it silently re-arms every job that was deliberately parked.
//! - `_in_progress` holds live claims. Deleting a claim whose worker is still
//!   checking in is not a cleanup; the worker carries on encoding and nothing
//!   will ever reconcile the result.
//!
//! So the command still empties all four by default - that is what its name has
//! always promised, and narrowing the default silently would break anyone who
//! scripted it - but nothing is removed until the user has been shown, per
//! directory, what is in it and why it matters, and has said yes. `--only`
//! narrows a run, `--dry-run` prints the same report and removes nothing, and
//! `--yes` answers in advance for scripted use.
//!
//! **The report and the prompt go to stderr, not stdout.** A prompt is not a
//! diagnostic, but it is also not this command's output: `clean` produces no
//! machine-readable result, and the report exists to be read next to the
//! question it is asking. Splitting them across two streams would mean
//! `plexify clean > log` hides half of a question that is still waiting for an
//! answer. They are printed rather than logged for the same reason - a
//! confirmation prompt that `RUST_LOG` can filter out is a prompt that can go
//! missing.

use anyhow::{anyhow, Context, Result};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use tokio::fs as async_fs;

use crate::job::Job;
use crate::queue::{heartbeat_path_for, is_stale, JobQueue, QueueDirectory, STALE_AFTER};

/// The worker log `work` writes beside the media root.
const WORKER_LOG: &str = "_worker.log";

/// Command to empty a work root's queue directories.
pub struct CleanCommand {
    media_root: PathBuf,
    work_root: PathBuf,
    /// Which queue directories to empty.
    targets: Vec<QueueDirectory>,
    /// Whether `targets` is the full default set rather than an explicit
    /// `--only`. Only a full clean takes the worker log with it; a run narrowed
    /// to one queue directory should touch exactly that.
    full: bool,
    dry_run: bool,
    assume_yes: bool,
    force: bool,
}

/// What a clean run did, so a caller (and a test) can tell the four apart
/// without reading the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanOutcome {
    /// There was nothing in the work root to remove.
    NothingToDo,
    /// `--dry-run`: the report was printed and nothing was removed.
    Reported,
    /// The user was asked and said no. Nothing was removed.
    Declined,
    /// The directories named were removed.
    Removed(Vec<QueueDirectory>),
}

/// Everything that is about to be deleted, read off the disk.
///
/// Built before anything is removed and rendered into the report. Deliberately
/// data rather than text, so the decision to refuse a live claim is made on the
/// same reading the user is shown rather than on a second look at the disk.
///
/// **This overlaps with `QueueStatus` in #144 (`feat/queue-status`), which reads
/// the same four directories and is explicitly structured for other callers.**
/// It is duplicated here only because that branch is unmerged and this one is
/// cut from `main`. When #144 lands, `CleanPlan` should be built from a
/// `QueueStatus` rather than from its own walk - the counts, the claim liveness
/// and the parked-job attempt counts all come from there - leaving only the
/// worker log and the target selection here.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanPlan {
    /// One entry per selected queue directory that exists on disk.
    pub directories: Vec<DirectoryPlan>,
    /// The worker log, if a full clean would take it.
    pub worker_log: Option<PathBuf>,
}

/// What one queue directory holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPlan {
    pub directory: QueueDirectory,
    /// `.job` files. The `.lock` directories, `.heartbeat` files and `.chunks`
    /// directories that live alongside them are not jobs and are not counted.
    pub jobs: usize,
    /// Claims in `_in_progress`, each with whether its worker is still alive.
    /// Empty for the other three.
    pub claims: Vec<Claim>,
    /// Parked jobs in `_failed`, with the attempt count that parked them. Empty
    /// for the other three.
    pub parked: Vec<Parked>,
}

/// A job some worker has claimed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub input_path: PathBuf,
    /// Whether the claiming worker has checked in within [`STALE_AFTER`].
    ///
    /// Judged by [`is_stale`], the same rule the startup sweep uses. A claim
    /// that is not stale is one a worker is still running.
    pub live: bool,
}

/// A job parked in `_failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parked {
    pub input_path: PathBuf,
    pub attempts: u32,
}

impl CleanPlan {
    /// Whether there is nothing at all to remove.
    pub fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.worker_log.is_none()
    }

    /// Claims whose worker is still checking in.
    pub fn live_claims(&self) -> impl Iterator<Item = &Claim> {
        self.directories
            .iter()
            .flat_map(|d| d.claims.iter())
            .filter(|claim| claim.live)
    }
}

/// Where the answer to the confirmation comes from.
///
/// A trait so that "the user said no" and "there is no user" are both testable
/// without a terminal. The two questions are separate on purpose: a run with
/// nobody to ask must refuse, not block, and the only way to be sure of that in
/// a test is for the test double to fail if it is asked at all.
pub trait Confirmation {
    /// Whether there is anyone to answer.
    fn is_interactive(&self) -> bool;

    /// Ask, and return whether the answer was yes.
    fn ask(&mut self, question: &str) -> Result<bool>;
}

/// Reads the answer from the terminal.
pub struct TerminalConfirmation;

impl Confirmation for TerminalConfirmation {
    fn is_interactive(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn ask(&mut self, question: &str) -> Result<bool> {
        let mut stderr = std::io::stderr();
        write!(stderr, "{question}").context("Could not print the confirmation prompt")?;
        stderr
            .flush()
            .context("Could not print the confirmation prompt")?;

        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("Could not read the confirmation answer")?;

        // Anything that is not an explicit yes is a no. This is the destructive
        // direction, so the default has to be the one that keeps the files.
        let answer = answer.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
}

impl CleanCommand {
    /// A full clean: all four queue directories, and the worker log.
    pub fn new(media_root: PathBuf, work_root: PathBuf) -> Self {
        Self {
            media_root,
            work_root,
            targets: QueueDirectory::ALL.to_vec(),
            full: true,
            dry_run: false,
            assume_yes: false,
            force: false,
        }
    }

    /// Narrow the run to the given queue directories. An empty list leaves the
    /// full default set in place, so `--only` with no values is not a way to
    /// delete nothing by accident.
    pub fn only(mut self, directories: &[QueueDirectory]) -> Self {
        if !directories.is_empty() {
            let mut targets = directories.to_vec();
            targets.sort();
            targets.dedup();
            self.targets = targets;
            self.full = false;
        }
        self
    }

    /// Print the report and remove nothing.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    /// Answer the confirmation in advance, for scripted use.
    pub fn assume_yes(mut self, assume_yes: bool) -> Self {
        self.assume_yes = assume_yes;
        self
    }

    /// Also allow deleting a claim whose worker is still alive.
    ///
    /// Separate from [`Self::assume_yes`] on purpose: a live claim is the one
    /// case where this is not a cleanup but a corruption, so it takes more than
    /// a `yes` to get past.
    pub fn force(mut self, force: bool) -> Self {
        self.force = force;
        self
    }

    pub async fn execute(&self) -> Result<()> {
        self.run(&mut TerminalConfirmation).await.map(|_| ())
    }

    /// The whole command, with the source of the confirmation injected.
    ///
    /// The order of the checks is the point. The report is printed before
    /// anything can refuse, so a user told "no" also sees what they were told
    /// "no" about; the live-claim refusal comes before the prompt, because a
    /// question a user can answer wrongly is not a safeguard; and the
    /// no-terminal refusal comes last, so a scripted run that would have been
    /// refused anyway is refused for the right reason.
    pub async fn run<C: Confirmation>(&self, confirm: &mut C) -> Result<CleanOutcome> {
        if !self.media_root.exists() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.media_root
            ));
        }

        if !self.media_root.is_dir() {
            return Err(anyhow!("Path is not a directory: {:?}", self.media_root));
        }

        let queue = JobQueue::new(self.media_root.clone(), self.work_root.clone());
        let plan = self.plan(&queue).await?;

        if plan.is_empty() {
            eprintln!(
                "Nothing to clean: {} holds no queue directories and no worker log.",
                self.work_root.display()
            );
            return Ok(CleanOutcome::NothingToDo);
        }

        eprint!("{}", self.render(&plan));

        if self.dry_run {
            eprintln!("--dry-run: nothing was deleted.");
            return Ok(CleanOutcome::Reported);
        }

        let live = plan.live_claims().count();
        if live > 0 && !self.force {
            return Err(anyhow!(
                "Refusing to delete a live claim: {live} job(s) in _in_progress have a worker \
                 that checked in less than {}s ago. Deleting the claim leaves that worker \
                 encoding with nothing to reconcile the result against. Stop the worker, or \
                 pass --force to delete it anyway.",
                STALE_AFTER.as_secs()
            ));
        }

        if !self.assume_yes {
            if !confirm.is_interactive() {
                return Err(anyhow!(
                    "Refusing to delete without confirmation: stdin is not a terminal, so there \
                     is nobody to answer the prompt. Pass --yes to confirm in advance, or \
                     --dry-run to see what would be deleted."
                ));
            }

            if !confirm.ask("Delete all of this? [y/N] ")? {
                eprintln!("Cancelled. Nothing was deleted.");
                return Ok(CleanOutcome::Declined);
            }
        }

        queue.clean(&self.targets).await?;

        if let Some(worker_log) = &plan.worker_log {
            async_fs::remove_file(worker_log)
                .await
                .with_context(|| format!("Could not remove the worker log: {worker_log:?}"))?;
        }

        let removed: Vec<QueueDirectory> = plan.directories.iter().map(|d| d.directory).collect();
        eprintln!("Deleted.");
        Ok(CleanOutcome::Removed(removed))
    }

    /// Read what is on disk, without changing any of it.
    pub async fn plan(&self, queue: &JobQueue) -> Result<CleanPlan> {
        let mut directories = Vec::new();

        for directory in &self.targets {
            let path = queue.path_of(*directory);
            let Some(job_files) = job_files(path).await? else {
                continue;
            };

            let mut plan = DirectoryPlan {
                directory: *directory,
                jobs: job_files.len(),
                claims: Vec::new(),
                parked: Vec::new(),
            };

            // Only two of the four hold anything a user cannot get back by
            // re-scanning, and those are the two worth itemising. Reading every
            // job file in a drained `_completed` to print names nobody needs
            // would make the report slower the better the queue is doing.
            match directory {
                QueueDirectory::InProgress => {
                    for job_file in &job_files {
                        let Some(job) = read_job(job_file).await else {
                            continue;
                        };
                        let live =
                            !is_stale(job_file, &heartbeat_path_for(job_file), STALE_AFTER).await;
                        plan.claims.push(Claim {
                            input_path: job.input_path,
                            live,
                        });
                    }
                    plan.claims.sort_by(|a, b| a.input_path.cmp(&b.input_path));
                }
                QueueDirectory::Failed => {
                    for job_file in &job_files {
                        let Some(job) = read_job(job_file).await else {
                            continue;
                        };
                        plan.parked.push(Parked {
                            input_path: job.input_path,
                            attempts: job.attempts,
                        });
                    }
                    plan.parked.sort_by(|a, b| a.input_path.cmp(&b.input_path));
                }
                _ => {}
            }

            directories.push(plan);
        }

        // The worker log belongs to `work`, not to any one queue directory, so
        // only a full clean takes it. A run narrowed with `--only` touches
        // exactly what it names.
        let worker_log = self.media_root.join(WORKER_LOG);
        let worker_log = (self.full && worker_log.exists()).then_some(worker_log);

        Ok(CleanPlan {
            directories,
            worker_log,
        })
    }

    /// The report, as printed.
    pub fn render(&self, plan: &CleanPlan) -> String {
        let mut out = String::new();

        // The work root is printed first because `-w/--work-dir` defaults to the
        // current working directory. A user who is about to delete the wrong
        // queue is almost always in the wrong directory, and this is the line
        // that says so.
        out.push_str(&format!("Work root:  {}\n", self.work_root.display()));
        out.push_str(&format!("Media root: {}\n\n", self.media_root.display()));
        out.push_str("clean will permanently delete:\n\n");

        for directory in &plan.directories {
            out.push_str(&format!(
                "  {:<14} {:>4} {:<5} - {}\n",
                directory.directory.on_disk_name(),
                directory.jobs,
                plural(directory.jobs, "job", "jobs"),
                consequence(directory.directory),
            ));
        }

        if plan.worker_log.is_some() {
            out.push_str(&format!(
                "  {:<14} {:>4} {:<5} - {}\n",
                WORKER_LOG, 1, "file", "the log `work` writes beside the media root",
            ));
        }

        for directory in &plan.directories {
            if !directory.claims.is_empty() {
                out.push_str("\nClaimed jobs in _in_progress:\n");
                for claim in &directory.claims {
                    out.push_str(&format!(
                        "  {:<10} {}\n",
                        if claim.live { "[live]" } else { "[stranded]" },
                        claim.input_path.display()
                    ));
                }
            }

            if !directory.parked.is_empty() {
                out.push_str("\nParked jobs in _failed:\n");
                for parked in &directory.parked {
                    out.push_str(&format!(
                        "  {} {:<5} {}\n",
                        parked.attempts,
                        plural(parked.attempts as usize, "attempt", "attempts"),
                        parked.input_path.display()
                    ));
                }
            }
        }

        out.push('\n');
        out
    }
}

/// Why the contents of a queue directory are worth a moment's thought.
fn consequence(directory: QueueDirectory) -> &'static str {
    match directory {
        QueueDirectory::Queue => "waiting to be transcoded; rebuilt by running scan again",
        QueueDirectory::InProgress => "claims held by workers, live or stranded",
        QueueDirectory::Completed => "the record that these files were transcoded",
        QueueDirectory::Failed => "why they failed, and what keeps scan from re-queueing them",
    }
}

fn plural(count: usize, one: &'static str, many: &'static str) -> &'static str {
    if count == 1 {
        one
    } else {
        many
    }
}

/// The `.job` files in a queue directory, or `None` if it is not there at all.
///
/// Only `.job` files count. `_queue` also holds the `.lock` directories enqueue
/// uses and `_in_progress` holds `.heartbeat` files and `.chunks` directories;
/// none of those is a job, and counting one would overstate the report.
async fn job_files(directory: &Path) -> Result<Option<Vec<PathBuf>>> {
    let mut entries = match async_fs::read_dir(directory).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow!("Could not read {directory:?}: {e}")),
    };

    let mut paths = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("Could not read {directory:?}"))?
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("job") && path.is_file() {
            paths.push(path);
        }
    }

    paths.sort();
    Ok(Some(paths))
}

/// Read a job file, or `None` if it has gone or cannot be parsed.
///
/// A job that vanished between the listing and the read is a worker's
/// `complete()` doing its job, and a job file nothing can parse is still a file
/// that will be deleted - it is counted either way. Neither is a reason to
/// refuse to report; both only cost a line of detail in the report.
async fn read_job(path: &Path) -> Option<Job> {
    let content = async_fs::read_to_string(path).await.ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
    use tempfile::TempDir;

    /// Says yes, and records that it was asked.
    struct Accepts {
        asked: bool,
    }

    impl Confirmation for Accepts {
        fn is_interactive(&self) -> bool {
            true
        }
        fn ask(&mut self, _question: &str) -> Result<bool> {
            self.asked = true;
            Ok(true)
        }
    }

    /// Says no.
    struct Declines {
        asked: bool,
    }

    impl Confirmation for Declines {
        fn is_interactive(&self) -> bool {
            true
        }
        fn ask(&mut self, _question: &str) -> Result<bool> {
            self.asked = true;
            Ok(false)
        }
    }

    /// A run with nobody to ask. Being asked at all is the failure this stands
    /// in for: on a real pipe `read_line` would block forever, and a test that
    /// blocked forever would be a hung suite rather than a failing one.
    struct NoTerminal {
        asked: bool,
    }

    impl Confirmation for NoTerminal {
        fn is_interactive(&self) -> bool {
            false
        }
        fn ask(&mut self, _question: &str) -> Result<bool> {
            self.asked = true;
            panic!("a non-interactive run asked for confirmation; a real one would have hung here");
        }
    }

    fn job_for(input: &str) -> Job {
        Job::new(
            PathBuf::from(input),
            MediaFileType::Mkv,
            QualitySettings::default(),
            PostProcessingSettings::default(),
            Path::new("/media"),
        )
    }

    /// A work root with one job in each of the four directories.
    async fn populated_work_root() -> TempDir {
        let temp = TempDir::new().unwrap();
        let queue = JobQueue::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        queue.init().await.unwrap();

        for (directory, input) in [
            (QueueDirectory::Queue, "/media/Waiting.mkv"),
            (QueueDirectory::InProgress, "/media/Claimed.mkv"),
            (QueueDirectory::Completed, "/media/Done.mkv"),
            (QueueDirectory::Failed, "/media/Broken.mkv"),
        ] {
            let mut job = job_for(input);
            if directory == QueueDirectory::Failed {
                job.attempts = 3;
                job.last_error = Some("ffmpeg exited with 1".to_string());
            }
            let path = queue.path_of(directory).join(job.job_filename());
            async_fs::write(&path, serde_json::to_string_pretty(&job).unwrap())
                .await
                .unwrap();
        }

        // A lock directory and a heartbeat sit alongside real jobs; neither is a
        // job and neither may be counted as one.
        async_fs::create_dir(queue.queue_dir.join("stale.job.lock"))
            .await
            .unwrap();
        async_fs::write(
            queue.in_progress_dir.join("orphan.job.heartbeat"),
            b"" as &[u8],
        )
        .await
        .unwrap();

        temp
    }

    fn command_for(temp: &TempDir) -> CleanCommand {
        CleanCommand::new(temp.path().to_path_buf(), temp.path().to_path_buf())
    }

    /// Whether the four queue directories still exist.
    fn queue_dirs_exist(temp: &TempDir) -> bool {
        QueueDirectory::ALL
            .iter()
            .all(|d| temp.path().join(d.on_disk_name()).exists())
    }

    #[tokio::test]
    async fn test_clean_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let clean_cmd = command_for(&temp_dir).assume_yes(true);

        assert!(clean_cmd.execute().await.is_ok());
    }

    #[tokio::test]
    async fn test_clean_nonexistent_directory() {
        let clean_cmd =
            CleanCommand::new(PathBuf::from("/nonexistent/path"), PathBuf::from("/tmp"));

        assert!(clean_cmd.execute().await.is_err());
    }

    #[tokio::test]
    async fn the_preview_lists_what_is_actually_there() {
        let temp = populated_work_root().await;
        let command = command_for(&temp);
        let queue = JobQueue::new(temp.path().to_path_buf(), temp.path().to_path_buf());

        let plan = command.plan(&queue).await.unwrap();
        let report = command.render(&plan);

        // Every directory is named, with the count of jobs actually in it - and
        // the lock directory and orphan heartbeat are not counted as jobs.
        for directory in QueueDirectory::ALL {
            let line = plan
                .directories
                .iter()
                .find(|d| d.directory == directory)
                .unwrap_or_else(|| panic!("{} missing from the plan", directory.on_disk_name()));
            assert_eq!(
                line.jobs,
                1,
                "{} counted {} jobs",
                directory.on_disk_name(),
                line.jobs
            );
            assert!(
                report.contains(directory.on_disk_name()),
                "report does not mention {}:\n{report}",
                directory.on_disk_name()
            );
        }

        // The two directories holding something a user cannot reconstruct are
        // itemised, and the other two are not.
        assert!(report.contains("Claimed.mkv"), "{report}");
        assert!(report.contains("Broken.mkv"), "{report}");
        assert!(report.contains("3 attempts"), "{report}");
        assert!(!report.contains("Done.mkv"), "{report}");

        // A claim written just now is a worker still checking in.
        assert!(report.contains("[live]"), "{report}");
        assert_eq!(plan.live_claims().count(), 1);
    }

    #[tokio::test]
    async fn the_preview_reports_a_work_root_that_holds_nothing() {
        let temp = TempDir::new().unwrap();
        let command = command_for(&temp);
        let queue = JobQueue::new(temp.path().to_path_buf(), temp.path().to_path_buf());

        let plan = command.plan(&queue).await.unwrap();
        assert!(plan.is_empty());
        assert_eq!(
            command.run(&mut Accepts { asked: false }).await.unwrap(),
            CleanOutcome::NothingToDo
        );
    }

    #[tokio::test]
    async fn a_declined_confirmation_deletes_nothing() {
        let temp = populated_work_root().await;
        // A live claim would refuse before the prompt, so this run must get past
        // that to be a test of the prompt at all.
        let command = command_for(&temp).force(true);

        let mut confirm = Declines { asked: false };
        let outcome = command.run(&mut confirm).await.unwrap();

        assert_eq!(outcome, CleanOutcome::Declined);
        assert!(confirm.asked, "the user was never asked");
        assert!(
            queue_dirs_exist(&temp),
            "a declined confirmation removed queue directories"
        );
        assert!(
            temp.path().join("_queue/stale.job.lock").exists(),
            "a declined confirmation removed queue contents"
        );
    }

    #[tokio::test]
    async fn a_dry_run_deletes_nothing_and_never_asks() {
        let temp = populated_work_root().await;
        let command = command_for(&temp).dry_run(true);

        let mut confirm = NoTerminal { asked: false };
        let outcome = command.run(&mut confirm).await.unwrap();

        assert_eq!(outcome, CleanOutcome::Reported);
        assert!(!confirm.asked);
        assert!(queue_dirs_exist(&temp), "a dry run removed something");
    }

    #[tokio::test]
    async fn the_skip_prompt_flag_deletes_without_asking() {
        let temp = populated_work_root().await;
        async_fs::write(temp.path().join(WORKER_LOG), b"log" as &[u8])
            .await
            .unwrap();
        let command = command_for(&temp).assume_yes(true).force(true);

        // Nothing may consult the terminal on this path, so the double that
        // panics when asked is the right one to pass.
        let mut confirm = NoTerminal { asked: false };
        let outcome = command.run(&mut confirm).await.unwrap();

        assert_eq!(outcome, CleanOutcome::Removed(QueueDirectory::ALL.to_vec()));
        assert!(!confirm.asked);
        for directory in QueueDirectory::ALL {
            assert!(
                !temp.path().join(directory.on_disk_name()).exists(),
                "{} survived a confirmed clean",
                directory.on_disk_name()
            );
        }
        assert!(!temp.path().join(WORKER_LOG).exists());
    }

    #[tokio::test]
    async fn a_non_interactive_run_without_the_flag_refuses_instead_of_waiting() {
        let temp = populated_work_root().await;
        let command = command_for(&temp).force(true);

        // `NoTerminal::ask` panics, so this test fails rather than hangs if the
        // refusal is ever removed.
        let mut confirm = NoTerminal { asked: false };
        let error = command.run(&mut confirm).await.unwrap_err().to_string();

        assert!(error.contains("stdin is not a terminal"), "{error}");
        assert!(error.contains("--yes"), "{error}");
        assert!(!confirm.asked);
        assert!(queue_dirs_exist(&temp));
    }

    #[tokio::test]
    async fn a_live_claim_is_refused_rather_than_asked_about() {
        let temp = populated_work_root().await;
        let command = command_for(&temp);

        let mut confirm = Accepts { asked: false };
        let error = command.run(&mut confirm).await.unwrap_err().to_string();

        assert!(error.contains("live claim"), "{error}");
        assert!(error.contains("--force"), "{error}");
        assert!(
            !confirm.asked,
            "a live claim was put to the user as a question"
        );
        assert!(queue_dirs_exist(&temp));
    }

    #[tokio::test]
    async fn a_live_claim_is_refused_even_with_the_skip_prompt_flag() {
        let temp = populated_work_root().await;
        // `--yes` is an answer to the prompt, and the live-claim refusal is not
        // a prompt. Deleting a claim a worker is still holding takes --force.
        let command = command_for(&temp).assume_yes(true);

        let error = command
            .run(&mut Accepts { asked: false })
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("live claim"), "{error}");
        assert!(queue_dirs_exist(&temp));
    }

    #[tokio::test]
    async fn a_stranded_claim_is_not_a_live_one() {
        let temp = TempDir::new().unwrap();
        let queue = JobQueue::new(temp.path().to_path_buf(), temp.path().to_path_buf());
        queue.init().await.unwrap();

        let job = job_for("/media/Stranded.mkv");
        let path = queue.in_progress_dir.join(job.job_filename());
        async_fs::write(&path, serde_json::to_string_pretty(&job).unwrap())
            .await
            .unwrap();
        // Backdate past STALE_AFTER: the worker that claimed this is gone, so
        // there is nothing to corrupt by deleting the claim.
        let long_ago =
            std::time::SystemTime::now() - STALE_AFTER - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(long_ago))
            .unwrap();

        let command = command_for(&temp);
        let plan = command.plan(&queue).await.unwrap();

        assert_eq!(plan.live_claims().count(), 0);
        let report = command.render(&plan);
        assert!(report.contains("[stranded]"), "{report}");

        // A stranded claim needs no --force: nothing is holding it.
        assert_eq!(
            command.run(&mut Accepts { asked: false }).await.unwrap(),
            CleanOutcome::Removed(QueueDirectory::ALL.to_vec())
        );
    }

    #[tokio::test]
    async fn only_narrows_the_run_to_what_it_names() {
        let temp = populated_work_root().await;
        async_fs::write(temp.path().join(WORKER_LOG), b"log" as &[u8])
            .await
            .unwrap();
        let command = command_for(&temp)
            .only(&[QueueDirectory::Completed])
            .assume_yes(true);

        let outcome = command.run(&mut Accepts { asked: false }).await.unwrap();

        assert_eq!(
            outcome,
            CleanOutcome::Removed(vec![QueueDirectory::Completed])
        );
        assert!(!temp.path().join("_completed").exists());
        assert!(temp.path().join("_queue").exists());
        assert!(temp.path().join("_in_progress").exists());
        assert!(temp.path().join("_failed").exists());
        // A narrowed run is not a full clean, so the worker log stays.
        assert!(temp.path().join(WORKER_LOG).exists());
    }

    #[tokio::test]
    async fn a_run_narrowed_away_from_in_progress_ignores_a_live_claim() {
        let temp = populated_work_root().await;
        let command = command_for(&temp)
            .only(&[QueueDirectory::Queue])
            .assume_yes(true);

        // The live claim is real, but this run is not going to touch it.
        let outcome = command.run(&mut Accepts { asked: false }).await.unwrap();

        assert_eq!(outcome, CleanOutcome::Removed(vec![QueueDirectory::Queue]));
        assert!(temp.path().join("_in_progress").exists());
    }
}
