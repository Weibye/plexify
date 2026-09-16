use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::fix::FixOutcome;
use crate::ignore::IgnoreFilter;
use crate::naming::{
    assess, scope_for, series_directory_disagreement, Assessment, LibraryRoot, Scope,
};
use crate::paths::to_forward_slashes;

/// Media file extensions that should be validated
pub const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "webm", "mov", "m4v"];

/// Something the library holds that is not in canonical form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// The file as it exists, relative to the media root, with `/` separators.
    pub path: String,
    pub kind: IssueKind,
}

/// What is wrong with a file, and what can be done about it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum IssueKind {
    /// The canonical form of this file differs from where it is now.
    Rename {
        /// Where the file belongs, relative to the media root.
        destination: String,
    },
    /// The path could not be decomposed, so no destination can be proposed.
    NeedsDecision { reason: String },
}

/// Something worth knowing about a file that is not a verdict on it.
///
/// A note proposes nothing and nothing acts on one. It exists because the
/// report is read by a person who can see what the tool cannot.
///
/// A note is not a substitute for a verdict either, so the same path can carry
/// both: a file sitting in a disagreeing series directory can still need a
/// rename for a reason that has nothing to do with the directory. A note must
/// therefore never state what the report as a whole proposes for its path -
/// only the report knows that, and only by reading its own [`ValidationReport::issues`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValidationNote {
    /// The file as it exists, relative to the media root, with `/` separators.
    pub path: String,
    pub kind: NoteKind,
}

/// What a note has to say.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NoteKind {
    /// The series directory holding this file names a different series than
    /// the file does. Neither name is assumed correct, so the directory is
    /// left alone.
    SeriesDirectoryDisagrees { directory: String, series: String },
}

impl NoteKind {
    /// A one-line explanation, for the validation report.
    ///
    /// It says what was observed and what is left alone because of it, and
    /// stops there. What the report proposes for the file is a separate
    /// question with a separate answer - see [`ValidationReport::proposal_for`].
    pub fn explain(&self) -> String {
        match self {
            NoteKind::SeriesDirectoryDisagrees { directory, series } => format!(
                "the directory says '{directory}' and the file says '{series}'; the directory is left as it is, because the path does not say which is right"
            ),
        }
    }
}

/// Validation report containing all issues found
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub scanned_files: usize,
    pub issues: Vec<ValidationIssue>,
    /// Observations that are not proposals. Nothing in `fix` reads these.
    #[serde(default)]
    pub notes: Vec<ValidationNote>,
    /// The directory every path in `issues` is relative to.
    pub library_root: PathBuf,
    /// The subtree that was actually walked. Differs from the root when a run
    /// was narrowed to one series or season.
    pub scan_path: PathBuf,
    pub validation_time: Duration,
}

impl ValidationReport {
    /// Files whose canonical destination differs from where they are.
    pub fn renames(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues
            .iter()
            .filter(|issue| matches!(issue.kind, IssueKind::Rename { .. }))
    }

    /// Files a person has to resolve.
    pub fn needing_decision(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues
            .iter()
            .filter(|issue| matches!(issue.kind, IssueKind::NeedsDecision { .. }))
    }

    /// What this report proposes for a path, if anything.
    ///
    /// A note and an issue are answers to different questions, so one path can
    /// carry both. Anything rendering a note has to ask this rather than assume
    /// the answer: a note that asserts nothing was proposed contradicts the
    /// renames listed above it in the same report.
    pub fn proposal_for(&self, path: &str) -> Option<&IssueKind> {
        self.issues
            .iter()
            .find(|issue| issue.path == path)
            .map(|issue| &issue.kind)
    }
}

/// Command to validate library naming conformity
pub struct ValidateCommand {
    scope: Scope,
    /// Whether the caller intends to act on the report. Validation itself is
    /// read-only either way; this only decides what the report says it is.
    fixing: bool,
}

impl ValidateCommand {
    /// Create a new validate command.
    ///
    /// The path may be the library root or any directory inside it; a narrower
    /// path narrows the run without changing what canonical means.
    pub fn new(path: PathBuf) -> Self {
        // Resolve first: a relative path such as `Series/Elementary` has no
        // components before the root, which would leave nothing to measure
        // against and no directory for the ignore filter to walk.
        let absolute = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        };

        Self {
            scope: scope_for(&absolute),
            fixing: false,
        }
    }

    /// Create a validate command whose library root the user stated.
    ///
    /// `scope_for` infers the root from the shape of the tree and refuses one
    /// shape it cannot decide - a media root named after a library root that
    /// holds exactly one. This is how a user says which reading is right, and it
    /// replaces the inference rather than tuning it. See [`Scope::stated`].
    pub fn rooted_at(path: PathBuf, library_root: PathBuf) -> Result<Self> {
        Ok(Self {
            scope: Scope::stated(&library_root, &path)?,
            fixing: false,
        })
    }

    /// State that the report will be acted on, so it does not describe itself
    /// as a dry run.
    pub fn fixing(mut self) -> Self {
        self.fixing = true;
        self
    }

    /// Execute the validation command
    pub async fn execute(&self) -> Result<ValidationReport> {
        let start_time = Instant::now();

        if !self.scope.scan_path.exists() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.scope.scan_path
            ));
        }

        if !self.scope.scan_path.is_dir() {
            return Err(anyhow!(
                "Path is not a directory: {:?}",
                self.scope.scan_path
            ));
        }

        info!(
            "🔍 Validating library naming in: {:?}",
            self.scope.scan_path
        );
        if !self.scope.is_whole_library() {
            info!(
                "📐 Judging against library root: {:?}",
                self.scope.library_root
            );
        }
        info!("📁 Recursively scanning all subdirectories...");

        // Initialize ignore filter
        // Rules belong to the library root even when the run is narrower, but only
        // the files above and inside the scanned subtree can affect it - see
        // `IgnoreFilter::for_scope`.
        let ignore_filter =
            match IgnoreFilter::for_scope(self.scope.library_root.clone(), &self.scope.scan_path) {
                Ok(filter) => Some(filter),
                Err(e) => {
                    warn!("Failed to load .plexifyignore patterns: {}", e);
                    None
                }
            };

        // Create a lookup set for media extensions for faster checks
        let media_extensions: std::collections::HashSet<&str> =
            MEDIA_EXTENSIONS.iter().copied().collect();

        // First, collect all media files with progress indicator
        let mut media_files = Vec::new();
        let mut ignored_count = 0;
        let mut files_processed = 0;

        let scan_pb = ProgressBar::new_spinner();
        scan_pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        scan_pb.set_message("Collecting media files...");
        scan_pb.enable_steady_tick(std::time::Duration::from_millis(120));

        for entry in WalkDir::new(&self.scope.scan_path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let path = e.path();

                // Always allow the root directory
                if path == self.scope.scan_path {
                    return true;
                }

                // Check if we should skip this directory and all its contents
                if path.is_dir() {
                    if let Some(ref filter) = ignore_filter {
                        if filter.should_skip_dir(path) {
                            debug!("🚫 Skipping entire directory: {:?}", path);
                            return false; // This will cause WalkDir to skip the directory
                        }
                    }
                }

                true // Allow files and non-ignored directories
            })
            .filter_map(|e| e.ok())
        {
            let path = entry.path();

            // Skip directories and non-media files
            if path.is_dir() {
                continue;
            }

            // Check if this individual file should be ignored
            if let Some(ref filter) = ignore_filter {
                if filter.should_ignore(path) {
                    debug!("🚫 Ignoring path: {:?}", path);
                    ignored_count += 1;
                    continue;
                }
            }

            files_processed += 1;

            // Update progress message periodically
            if files_processed % 500 == 0 {
                scan_pb.set_message(format!("Scanned {} files...", files_processed));
            }

            // Check if it's a media file
            if let Some(extension) = path.extension() {
                let ext = extension.to_string_lossy().to_lowercase();
                if media_extensions.contains(ext.as_str()) {
                    media_files.push(path.to_path_buf());
                }
            }
        }

        scan_pb.finish_and_clear();

        info!(
            "🔍 Found {} media files, validating in parallel...",
            media_files.len()
        );

        if ignored_count > 0 {
            info!(
                "📋 Ignored {} paths due to .plexifyignore patterns",
                ignored_count
            );
        }
        // Create validation progress bar
        let validate_pb = ProgressBar::new(media_files.len() as u64);
        validate_pb.set_style(
            ProgressStyle::with_template("Validating {bar:30.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        validate_pb.set_message("files");

        // Create shared reference to self for parallel processing
        let library_root = Arc::new(&self.scope.library_root);
        let pb = Arc::new(validate_pb);

        // Process files in parallel using rayon
        let assessed: Vec<(Option<ValidationIssue>, Option<ValidationNote>)> = media_files
            .par_iter()
            .filter_map(|path| {
                let relative_path = match path.strip_prefix(library_root.as_ref()) {
                    Ok(rel_path) => rel_path,
                    Err(_) => return None,
                };

                let result = (
                    self.assess_file(relative_path),
                    self.note_for(relative_path),
                );
                pb.inc(1);
                Some(result)
            })
            .collect();

        let mut issues: Vec<ValidationIssue> = Vec::new();
        let mut notes: Vec<ValidationNote> = Vec::new();
        for (issue, note) in assessed {
            issues.extend(issue);
            notes.extend(note);
        }

        // Rayon returns work in whatever order it finished; a report is read
        // top to bottom, so order it the way the library is laid out.
        issues.sort_by(|left, right| left.path.cmp(&right.path));
        notes.sort_by(|left, right| left.path.cmp(&right.path));

        pb.finish_and_clear();

        let validation_time = start_time.elapsed();

        let report = ValidationReport {
            scanned_files: media_files.len(),
            issues,
            notes,
            library_root: self.scope.library_root.clone(),
            scan_path: self.scope.scan_path.clone(),
            validation_time,
        };

        info!(
            "✅ Validation complete. Scanned {} files, found {} issues in {:.2}s",
            report.scanned_files,
            report.issues.len(),
            validation_time.as_secs_f64()
        );

        Ok(report)
    }

    /// Assess a single file against the canonical naming form.
    fn assess_file(&self, relative_path: &std::path::Path) -> Option<ValidationIssue> {
        let path = to_forward_slashes(relative_path);

        match assess(relative_path) {
            Assessment::Canonical => None,
            Assessment::Rename { destination } => Some(ValidationIssue {
                path,
                kind: IssueKind::Rename { destination },
            }),
            Assessment::Unresolvable(reason) => Some(ValidationIssue {
                path,
                kind: IssueKind::NeedsDecision {
                    reason: reason.reason(),
                },
            }),
        }
    }

    /// Note anything about a file that is worth reading but is not a verdict.
    ///
    /// Deliberately separate from `assess_file`: a file can be canonical and
    /// still carry a note, and a file can need a rename *and* carry one, so a
    /// note is never one of the outcomes an assessment chooses between.
    ///
    /// The one overlap that cannot occur is a note on a path reported as
    /// `NeedsDecision`, and that holds only because both functions call the same
    /// pure `parse` on the same string, so a path one refuses the other refuses
    /// too. A `NoteKind` that read the path itself instead would not inherit it.
    fn note_for(&self, relative_path: &std::path::Path) -> Option<ValidationNote> {
        let disagreement = series_directory_disagreement(relative_path)?;

        Some(ValidationNote {
            path: to_forward_slashes(relative_path),
            kind: NoteKind::SeriesDirectoryDisagrees {
                directory: disagreement.directory,
                series: disagreement.series,
            },
        })
    }

    /// Print what a fix run did to stdout
    pub fn print_fix_outcome(&self, outcome: &FixOutcome) {
        print!("{}", self.render_fix_outcome(outcome));
    }

    /// Render the result of a fix run.
    ///
    /// The renames that succeeded are not listed again - the report printed
    /// immediately above named every one of them, and the plan file records
    /// them. What is worth reading here is what did *not* happen.
    pub fn render_fix_outcome(&self, outcome: &FixOutcome) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        let _ = writeln!(
            out,
            "
🔧 Fix"
        );
        let _ = writeln!(out, "──────");
        let _ = writeln!(out, "✅ Renamed: {}", outcome.applied.len());
        if !outcome.refusals.is_empty() {
            let _ = writeln!(out, "⛔ Refused: {}", outcome.refusals.len());
        }
        if !outcome.failed.is_empty() {
            let _ = writeln!(out, "❌ Failed: {}", outcome.failed.len());
        }
        let _ = writeln!(out, "📄 Plan: {}", to_forward_slashes(&outcome.plan_file));

        if !outcome.refusals.is_empty() {
            let _ = writeln!(
                out,
                "
⛔ Refused, and left exactly as they were:"
            );
            let _ = writeln!(out, "──────────────────────────────────────────");
            for refusal in &outcome.refusals {
                let _ = writeln!(
                    out,
                    "
  {}",
                    refusal.path
                );
                let _ = writeln!(out, "  {}", refusal.reason.explain());
            }
        }

        if !outcome.failed.is_empty() {
            let _ = writeln!(
                out,
                "
❌ Attempted and failed:"
            );
            let _ = writeln!(out, "────────────────────────");
            for failure in &outcome.failed {
                let _ = writeln!(
                    out,
                    "
  {}",
                    failure.attempted.from
                );
                let _ = writeln!(out, "  {}", failure.error);
            }
        }

        if !outcome.emptied_directories.is_empty() {
            let _ = writeln!(
                out,
                "
📁 Left empty by this run, and not removed:"
            );
            let _ = writeln!(out, "───────────────────────────────────────────");
            for directory in &outcome.emptied_directories {
                let _ = writeln!(out, "   {directory}");
            }
        }

        out
    }

    /// Print the validation report to stdout
    pub fn print_report(&self, report: &ValidationReport) {
        print!("{}", self.render_report(report));
    }

    /// Render the validation report.
    ///
    /// Paths are shown relative to the scanned root, which is printed once at the
    /// top: a rename is two paths that differ in one component, and that
    /// difference is what the reader is looking for.
    pub fn render_report(&self, report: &ValidationReport) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        let renames: Vec<_> = report.renames().collect();
        let decisions: Vec<_> = report.needing_decision().collect();

        let _ = writeln!(out, "\n📊 Library Naming Report");
        let _ = writeln!(out, "═══════════════════════");
        let _ = writeln!(
            out,
            "📂 Scanned directory: {}",
            to_forward_slashes(&report.scan_path)
        );
        // Only worth saying when the two differ: a scoped run judges paths
        // against a root it did not walk, and the reader should know which.
        if report.scan_path != report.library_root {
            let _ = writeln!(
                out,
                "📐 Library root: {}",
                to_forward_slashes(&report.library_root)
            );
        }
        let _ = writeln!(out, "📁 Files scanned: {}", report.scanned_files);
        let _ = writeln!(out, "✏️  Renames proposed: {}", renames.len());
        let _ = writeln!(out, "🤔 Needing a decision: {}", decisions.len());
        if !report.notes.is_empty() {
            let _ = writeln!(out, "📝 Also noted: {}", report.notes.len());
        }
        let _ = writeln!(
            out,
            "⏱️  Validation time: {:.2}s",
            report.validation_time.as_secs_f64()
        );

        if report.issues.is_empty() {
            let _ = writeln!(out, "\n✅ Every file is already in canonical form.");

            // A note is not an issue, so it survives a library that has none.
            if report.notes.is_empty() {
                return out;
            }
        }

        if !renames.is_empty() {
            let _ = writeln!(out, "\n✏️  Proposed renames:");
            let _ = writeln!(out, "─────────────────────");
            for issue in &renames {
                if let IssueKind::Rename { destination } = &issue.kind {
                    let _ = writeln!(out, "\n  {}", issue.path);
                    let _ = writeln!(out, "→ {destination}");
                }
            }
        }

        if !decisions.is_empty() {
            let _ = writeln!(out, "\n🤔 Needing a decision:");
            let _ = writeln!(out, "──────────────────────");
            for issue in &decisions {
                if let IssueKind::NeedsDecision { reason } = &issue.kind {
                    let _ = writeln!(out, "\n  {}", issue.path);
                    let _ = writeln!(out, "  {reason}");
                }
            }
        }

        if !report.notes.is_empty() {
            let _ = writeln!(out, "\n📝 Also noted:");
            let _ = writeln!(out, "──────────────");
            for note in &report.notes {
                let _ = writeln!(out, "\n  {}", note.path);
                let _ = writeln!(out, "  {}", note.kind.explain());
                // The note said what it observed; the report says what it
                // proposes for the same file, which is a different answer and
                // is not always "nothing".
                match report.proposal_for(&note.path) {
                    Some(IssueKind::Rename { destination }) => {
                        let _ = writeln!(out, "  this file is still proposed for rename:");
                        let _ = writeln!(out, "→ {destination}");
                    }
                    Some(IssueKind::NeedsDecision { reason }) => {
                        let _ = writeln!(out, "  this file also needs a decision: {reason}");
                    }
                    None => {
                        let _ = writeln!(out, "  nothing is proposed for this file.");
                    }
                }
            }
        }

        if report.issues.is_empty() {
            return out;
        }

        let _ = writeln!(out, "\n💡 Canonical form:");
        let _ = writeln!(out, "──────────────────");
        for root in LibraryRoot::all() {
            if root.is_episodic() {
                let _ = writeln!(
                    out,
                    "   {}/Show Name/Season NN/Show Name - SNNENN - Episode Title [quality].ext",
                    root.as_str()
                );
            } else {
                let _ = writeln!(
                    out,
                    "   {}/Film Name (Year)/Film Name (Year) [quality].ext",
                    root.as_str()
                );
            }
        }
        let _ = writeln!(
            out,
            "{}",
            if self.fixing {
                "\n   Carrying these out now."
            } else {
                "\n   Nothing has been changed on disk. Re-run with --fix to carry these out."
            }
        );

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// The single destination a report proposes, for tests that create one file.
    fn only_destination(report: &ValidationReport) -> String {
        match report.issues.as_slice() {
            [ValidationIssue {
                kind: IssueKind::Rename { destination },
                ..
            }] => destination.clone(),
            other => panic!("expected exactly one proposed rename, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_validate_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let validate_cmd = ValidateCommand::new(temp_dir.path().to_path_buf());

        let result = validate_cmd.execute().await;
        assert!(result.is_ok());

        let report = result.unwrap();
        assert_eq!(report.scanned_files, 0);
        assert_eq!(report.issues.len(), 0);
        assert!(report.validation_time > Duration::from_secs(0));
    }

    #[tokio::test]
    async fn test_validate_correct_tv_show() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named TV show (using Series instead of TV Shows)
        let tv_path = media_root.join("Series/Breaking Bad/Season 01");
        fs::create_dir_all(&tv_path).unwrap();
        fs::write(tv_path.join("Breaking Bad - S01E01 - Pilot.mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_correct_anime() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named anime
        let anime_path = media_root.join("Anime/Attack on Titan/Season 01");
        fs::create_dir_all(&anime_path).unwrap();
        fs::write(
            anime_path.join("Attack on Titan - S01E01 - To You, in 2000 Years.mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_correct_movie() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create correctly named movie
        let movie_path = media_root.join("Movies/The Dark Knight (2008)");
        fs::create_dir_all(&movie_path).unwrap();
        fs::write(movie_path.join("The Dark Knight (2008).mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0);
    }

    #[tokio::test]
    async fn test_validate_incorrect_naming() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create incorrectly named files
        fs::create_dir_all(media_root.join("Random")).unwrap();
        fs::write(media_root.join("Random/some_movie.mkv"), "").unwrap();

        fs::create_dir_all(media_root.join("Series/Show")).unwrap();
        fs::write(media_root.join("Series/Show/episode.mkv"), "").unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 2);
    }

    #[tokio::test]
    async fn test_validate_nonexistent_directory() {
        let validate_cmd = ValidateCommand::new(PathBuf::from("/nonexistent/path"));

        let result = validate_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_validate_with_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\n*.tmp\ntools",
        )
        .unwrap();

        // Create directory structure with media files
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();

        // Create media files - some should be ignored
        fs::write(media_root.join("Downloads/bad_movie.mkv"), "").unwrap();
        fs::write(media_root.join("tools/utility.mkv"), "").unwrap();
        fs::write(media_root.join("temp.tmp"), "").unwrap();
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();

        // Should only scan the non-ignored movie file
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0); // The movie is correctly named
    }

    #[tokio::test]
    async fn test_validate_with_nested_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create root .plexifyignore
        fs::write(media_root.join(".plexifyignore"), "*.tmp").unwrap();

        // Create nested directory with its own .plexifyignore
        fs::create_dir_all(media_root.join("Series/old")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();
        fs::write(media_root.join("Series/.plexifyignore"), "old/").unwrap();

        // Create test files
        fs::write(media_root.join("test.tmp"), "").unwrap();
        fs::write(media_root.join("Series/good_show.mkv"), "").unwrap();
        fs::write(media_root.join("Series/old/old_episode.mkv"), "").unwrap();
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();

        // Should scan 2 files: Series/good_show.mkv and the movie
        // Should ignore: test.tmp (root pattern), Series/old/old_episode.mkv (nested pattern)
        assert_eq!(report.scanned_files, 2);
        assert_eq!(report.issues.len(), 1); // Only Series/good_show.mkv has incorrect naming
    }

    #[tokio::test]
    async fn test_validate_series_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test 1: Simple case with TVDB id that should match "Alternative Series format"
        let series_path1 = media_root.join("Series/Critical Role (2015) {tvdb-296861}/Season 01");
        fs::create_dir_all(&series_path1).unwrap();
        fs::write(
            series_path1.join("Critical Role S01E01 Arrival at Kraghammer.mp4"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);

        assert_eq!(
            only_destination(&report),
            "Series/Critical Role (2015) {tvdb-296861}/Season 01/Critical Role - S01E01 - Arrival at Kraghammer.mp4",
            "the tvdb id and the year on the directory are kept as they are"
        );
    }

    #[tokio::test]
    async fn test_validate_complex_series_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test the exact case from the issue with extended season name and brackets
        let series_path =
            media_root.join("Series/Critical Role (2015) {tvdb-296861}/Season 01 - Vox Machina");
        fs::create_dir_all(&series_path).unwrap();
        fs::write(
            series_path.join("Critical Role - S01E01 - Arrival at Kraghammer - [1080p30].mp4"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);

        assert_eq!(
            only_destination(&report),
            "Series/Critical Role (2015) {tvdb-296861}/Season 01 - Vox Machina/Critical Role - S01E01 - Arrival at Kraghammer [1080p30].mp4",
            "the arc name stays on the directory; only the dash before the quality goes"
        );
    }

    #[tokio::test]
    async fn test_validate_anime_with_tvdb_id() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Test anime with TVDB id
        let anime_path = media_root.join("Anime/Attack on Titan {tvdb-123456}/Season 01");
        fs::create_dir_all(&anime_path).unwrap();
        fs::write(
            anime_path.join("Attack on Titan S01E01 To You in 2000 Years.mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;

        assert!(result.is_ok());
        let report = result.unwrap();
        assert_eq!(report.scanned_files, 1);
        assert_eq!(
            only_destination(&report),
            "Anime/Attack on Titan {tvdb-123456}/Season 01/Attack on Titan - S01E01 - To You in 2000 Years.mkv",
            "an anime directory carrying a tvdb id is left alone"
        );
    }

    #[tokio::test]
    async fn test_validate_skips_ignored_directories() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file that ignores entire directories
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\ntools/\n*.tmp",
        )
        .unwrap();

        // Create directory structure with many files in ignored directories
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Movies/Good Movie (2021)")).unwrap();

        // Create many media files in ignored directories (simulate the performance issue)
        for i in 0..100 {
            fs::write(media_root.join(format!("Downloads/video_{}.mkv", i)), "").unwrap();
            fs::write(media_root.join(format!("tools/tool_{}.mkv", i)), "").unwrap();
        }

        // Create some files that should be processed
        fs::write(media_root.join("temp.tmp"), "").unwrap(); // Should be ignored by pattern
        fs::write(
            media_root.join("Movies/Good Movie (2021)/Good Movie (2021).mkv"),
            "",
        )
        .unwrap();

        let validate_cmd = ValidateCommand::new(media_root.to_path_buf());
        let result = validate_cmd.execute().await;
        assert!(result.is_ok());
        let report = result.unwrap();

        // Should only scan 1 file (the movie), not the 200+ files in ignored directories
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.issues.len(), 0); // The movie is correctly named
    }

    #[test]
    fn render_report_shows_the_destination_next_to_the_current_path() {
        let root = PathBuf::from("C:/media").join("library");
        let command = ValidateCommand::new(root.clone());

        let report = ValidationReport {
            scanned_files: 2,
            issues: vec![
                ValidationIssue {
                    path: "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv"
                        .to_string(),
                    kind: IssueKind::Rename {
                        destination:
                            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
                                .to_string(),
                    },
                },
                ValidationIssue {
                    path: "Series/Veronica Mars/Series/Veronica Mars S02E04.mp4".to_string(),
                    kind: IssueKind::NeedsDecision {
                        reason: "'Series' appears twice in this path".to_string(),
                    },
                },
            ],
            notes: Vec::new(),
            library_root: root.clone(),
            scan_path: root,
            validation_time: Duration::from_secs(0),
        };

        let rendered = command.render_report(&report);

        assert!(
            !rendered.contains('\\'),
            "report should render every path with forward slashes: {rendered}"
        );
        assert!(rendered.contains("C:/media/library"));
        assert!(rendered.contains("Renames proposed: 1"));
        assert!(rendered.contains("Needing a decision: 1"));
        assert!(
            rendered.contains("Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"),
            "the destination belongs in the report: {rendered}"
        );
        assert!(rendered.contains("'Series' appears twice in this path"));
    }

    #[test]
    fn a_clean_library_says_so() {
        let root = PathBuf::from("C:/media");
        let command = ValidateCommand::new(root.clone());

        let report = ValidationReport {
            scanned_files: 12,
            issues: Vec::new(),
            notes: Vec::new(),
            library_root: root.clone(),
            scan_path: root,
            validation_time: Duration::from_secs(0),
        };

        assert!(command
            .render_report(&report)
            .contains("Every file is already in canonical form."));
    }

    /// The filename is what `naming` parses and what Plex reads, so a series
    /// directory naming something else is worth saying. It is only ever said:
    /// renaming the directory would move every file in it, and nothing in the
    /// path says which of the two names is right.
    #[tokio::test]
    async fn notes_a_series_directory_that_disagrees_without_proposing_anything() {
        let temp = TempDir::new().unwrap();
        let media_root = temp.path();

        let season = media_root.join("Series/Super Best Friends Play - FFX/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(
            season.join("Super Best Friends Play - Final Fantasy X - S01E13.webm"),
            "",
        )
        .unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(
            report.issues.is_empty(),
            "the file is already canonical and must not be touched: {:?}",
            report.issues
        );
        assert_eq!(
            report.notes,
            vec![ValidationNote {
                path: "Series/Super Best Friends Play - FFX/Season 01/Super Best Friends Play - Final Fantasy X - S01E13.webm".to_string(),
                kind: NoteKind::SeriesDirectoryDisagrees {
                    directory: "Super Best Friends Play - FFX".to_string(),
                    series: "Super Best Friends Play - Final Fantasy X".to_string(),
                },
            }]
        );

        let rendered = ValidateCommand::new(media_root.to_path_buf()).render_report(&report);
        assert!(
            rendered.contains("Also noted:"),
            "a library with nothing to rename must still show its notes: {rendered}"
        );
        assert!(rendered.contains("nothing is proposed for this file."));
        assert!(rendered.contains("'Super Best Friends Play - FFX'"));
        assert!(
            !rendered.contains("Re-run with --fix"),
            "a note is not something --fix carries out: {rendered}"
        );
    }

    /// A note and an issue answer different questions about one file, so the
    /// two must never contradict each other in the same report. The note is
    /// about the *directory*, which is left alone; what happens to the *file*
    /// comes from the report's own issues, and here it is a rename.
    #[tokio::test]
    async fn a_noted_file_that_is_also_renamed_says_so() {
        let temp = TempDir::new().unwrap();
        let media_root = temp.path();

        // Two faults at once: the directory names a different series, and the
        // season directory and marker are both off canonical form.
        let season = media_root.join("Series/FFX/Season 1");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Final Fantasy X - s01e13 - Zanarkand.webm"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        let note = match report.notes.as_slice() {
            [note] => note.clone(),
            other => panic!("expected exactly one note, got {other:?}"),
        };
        let destination = only_destination(&report);
        assert_eq!(
            report.proposal_for(&note.path),
            Some(&IssueKind::Rename {
                destination: destination.clone()
            }),
            "the noted path is the one being renamed, or this test measures nothing"
        );

        let rendered = ValidateCommand::new(media_root.to_path_buf()).render_report(&report);
        assert!(
            !rendered.contains("nothing is proposed"),
            "the report proposes a rename for this very path: {rendered}"
        );
        assert!(
            rendered.contains("this file is still proposed for rename:"),
            "the note must say what was proposed for its path: {rendered}"
        );
        assert!(
            rendered.matches(&destination).count() >= 2,
            "the destination belongs beside the note as well as in the rename list: {rendered}"
        );
    }

    /// The report-wide form of the same property: nothing in `notes` may claim
    /// something `issues` contradicts. Rendering a note reads `proposal_for`,
    /// so the only way the two can disagree is a noted path the report says is
    /// undecidable - which `note_for` cannot produce, because both it and
    /// `assess_file` refuse on the same `parse`.
    #[tokio::test]
    async fn no_noted_path_is_also_undecidable() {
        let temp = TempDir::new().unwrap();
        let media_root = temp.path();

        // A spread of shapes: canonical, renameable, undecidable, and each of
        // the first two in a directory that disagrees with its files.
        for (directory, file) in [
            ("Series/Breaking Bad/Season 01", "Breaking Bad - S01E01.mkv"),
            ("Series/FFX/Season 1", "Final Fantasy X - s01e13.webm"),
            ("Series/FFX/Season 01", "Final Fantasy X - S01E14.webm"),
            ("Series/Elementary/Season 02", "Elementary - S02E05.5.mkv"),
            ("Series/ELM/Season 02", "Elementary - S02E06.5.mkv"),
        ] {
            let season = media_root.join(directory);
            fs::create_dir_all(&season).unwrap();
            fs::write(season.join(file), "").unwrap();
        }

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(!report.notes.is_empty(), "the fixture must produce notes");
        assert!(
            report.needing_decision().next().is_some(),
            "the fixture must produce an undecidable path too"
        );

        for note in &report.notes {
            assert!(
                !matches!(
                    report.proposal_for(&note.path),
                    Some(IssueKind::NeedsDecision { .. })
                ),
                "a path cannot be both noted and undecidable: {note:?}"
            );
        }
    }

    /// A note is an observation, not a proposal, so nothing in `fix` reads it.
    #[tokio::test]
    async fn a_note_never_becomes_a_rename() {
        let temp = TempDir::new().unwrap();
        let media_root = temp.path();

        let season = media_root.join("Series/FFX/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Final Fantasy X - S01E13 - Zanarkand.webm"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();
        assert_eq!(report.notes.len(), 1);

        let plan = crate::fix::plan(&report);

        assert!(
            plan.moves.is_empty() && plan.refusals.is_empty(),
            "a disagreeing directory is reported, never acted on: {plan:?}"
        );
        assert!(season
            .join("Final Fantasy X - S01E13 - Zanarkand.webm")
            .exists());
    }

    #[tokio::test]
    async fn says_nothing_about_a_directory_the_files_agree_with() {
        let temp = TempDir::new().unwrap();
        let media_root = temp.path();

        let season = media_root.join("Series/Breaking Bad (2008) {tvdb-81189}/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Breaking Bad - S01E01 - Pilot.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert!(
            report.notes.is_empty(),
            "an annotation on the directory is not a different name: {:?}",
            report.notes
        );
    }

    #[tokio::test]
    async fn a_lowercase_episode_marker_is_reported_rather_than_accepted() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let season = media_root.join("Series/Breaking Bad/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Breaking Bad - s01e01 - Pilot.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(
            only_destination(&report),
            "Series/Breaking Bad/Season 01/Breaking Bad - S01E01 - Pilot.mkv"
        );
    }

    #[tokio::test]
    async fn a_duplicated_library_root_is_left_for_a_person_to_resolve() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let nested = media_root.join("Series/Veronica Mars/Series/Veronica Mars S02E04/Season 01");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Veronica Mars S02E04.mp4"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(
            report.renames().count(),
            0,
            "no destination may be proposed"
        );
        assert_eq!(report.needing_decision().count(), 1);
    }

    /// Issue #137, end to end: a canonical library whose media root is named
    /// after a library root was refused in its entirety, leaving `--fix` inert.
    #[tokio::test]
    async fn a_media_root_named_after_a_library_root_does_not_refuse_the_library() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path().join("home").join("bob").join("Movies");

        let season = media_root.join("Series/Elementary/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Elementary - S01E01 - Pilot.mkv"), "").unwrap();

        let film = media_root.join("Movies/Batman Begins (2005)");
        fs::create_dir_all(&film).unwrap();
        fs::write(film.join("Batman Begins (2005).mkv"), "").unwrap();

        fs::create_dir_all(media_root.join("Anime")).unwrap();

        let report = ValidateCommand::new(media_root.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.library_root, media_root);
        assert_eq!(report.scanned_files, 2);
        assert_eq!(
            report.needing_decision().count(),
            0,
            "both files are canonical; nothing here needs a person"
        );
        assert_eq!(report.renames().count(), 0);
    }

    /// A film directory named `Series` inside a real `Movies` root, with the run
    /// narrowed to that root.
    ///
    /// Nothing here may be proposed for renaming. A `library_root` one level too
    /// deep reads the film directory as a `Series` library, and the episode
    /// inside it gets a well-formed destination that `fix.rs` has no way to
    /// question - `--fix` would build `Season 01/` inside a film folder and move
    /// a file into it. Refusing is the only safe answer, and it is what a
    /// whole-library run over the same tree already does.
    #[tokio::test]
    async fn a_film_directory_named_after_a_root_is_refused_rather_than_renamed() {
        let temp_dir = TempDir::new().unwrap();
        let movies = temp_dir.path().join("lib").join("Movies");

        let film_named_series = movies.join("Series");
        fs::create_dir_all(&film_named_series).unwrap();
        fs::write(film_named_series.join("Series (2019).mkv"), "").unwrap();
        fs::write(
            film_named_series.join("Elementary - S01E01 - Pilot.mkv"),
            "",
        )
        .unwrap();

        let film = movies.join("Batman Begins (2005)");
        fs::create_dir_all(&film).unwrap();
        fs::write(film.join("Batman Begins (2005).mkv"), "").unwrap();

        fs::create_dir_all(temp_dir.path().join("lib").join("Series")).unwrap();

        let report = ValidateCommand::new(movies.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.library_root, temp_dir.path().join("lib"));
        assert_eq!(
            report.renames().count(),
            0,
            "no file in a film directory may earn a destination from a root that is not one"
        );
        assert!(
            report
                .needing_decision()
                .any(|issue| issue.path.ends_with("Elementary - S01E01 - Pilot.mkv")),
            "the episode in the film directory is a duplicated root for a person to resolve"
        );
        assert!(
            !report
                .issues
                .iter()
                .any(|issue| issue.path.contains("Batman Begins")),
            "a film sitting in the Movies root is canonical and must not be reported at all"
        );
    }

    /// Issue #142: a media root named after a library root and holding exactly
    /// one, which the probe cannot tell from a duplication and so refuses.
    ///
    /// Both halves are asserted here, because the flag is only worth anything if
    /// the inference really does refuse this tree - a test that only exercised
    /// the flag would pass whether or not there was a problem to solve.
    #[tokio::test]
    async fn a_stated_library_root_resolves_a_tree_the_probe_refuses() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path().join("srv").join("Movies");
        let film = media_root.join("Movies").join("Batman Begins (2005)");
        fs::create_dir_all(&film).unwrap();
        fs::write(film.join("Batman Begins 2005.mkv"), "").unwrap();

        let inferred = ValidateCommand::new(media_root.clone())
            .execute()
            .await
            .unwrap();
        assert_eq!(
            inferred.needing_decision().count(),
            1,
            "the inference reads the media root as the Movies root, so the real one duplicates it"
        );
        assert_eq!(inferred.renames().count(), 0);

        let stated = ValidateCommand::rooted_at(media_root.clone(), media_root.clone())
            .unwrap()
            .execute()
            .await
            .unwrap();

        assert_eq!(
            stated.needing_decision().count(),
            0,
            "the root is stated, so nothing below it is a duplication"
        );
        assert_eq!(
            stated
                .renames()
                .map(|issue| issue.path.clone())
                .collect::<Vec<_>>(),
            vec!["Movies/Batman Begins (2005)/Batman Begins 2005.mkv".to_string()],
            "and the film below it is judged against that root"
        );
    }

    /// The paths reach the report as they were spelled, so what a user reads is
    /// what they can type back. Canonicalising them to make `strip_prefix` safe
    /// put a `\\?\` in front of every Windows path instead.
    #[tokio::test]
    async fn a_stated_root_reaches_the_report_as_it_was_given() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path().join("srv").join("Movies");
        fs::create_dir_all(media_root.join("Movies")).unwrap();

        let report = ValidateCommand::rooted_at(media_root.clone(), media_root.clone())
            .unwrap()
            .execute()
            .await
            .unwrap();

        assert_eq!(report.library_root, media_root);
        assert_eq!(report.scan_path, media_root);
    }

    /// The flag states a fact; it does not license a guess. A path outside the
    /// root it is given, and a root that cannot be read, are both refused.
    #[tokio::test]
    async fn a_stated_root_that_does_not_hold_the_path_is_refused() {
        let temp_dir = TempDir::new().unwrap();
        let library = temp_dir.path().join("library");
        let elsewhere = temp_dir.path().join("elsewhere");
        fs::create_dir_all(library.join("Series")).unwrap();
        fs::create_dir_all(&elsewhere).unwrap();

        let outside = ValidateCommand::rooted_at(elsewhere, library.clone());
        assert!(
            outside.is_err(),
            "a path outside the stated root describes no one tree"
        );

        let missing =
            ValidateCommand::rooted_at(library.join("Series"), temp_dir.path().join("nope"));
        assert!(
            missing.is_err(),
            "a root that cannot be read is a refusal, not a fallback to the inference"
        );
    }

    /// Narrowing a run under a stated root still judges from the root, which is
    /// the property the whole `Scope` split exists for.
    #[tokio::test]
    async fn a_stated_root_still_judges_a_narrowed_run_from_the_root() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path().join("srv").join("Series");
        let season = media_root
            .join("Series")
            .join("Elementary")
            .join("Season 6");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();

        let report = ValidateCommand::rooted_at(season, media_root)
            .unwrap()
            .execute()
            .await
            .unwrap();

        assert_eq!(
            report
                .renames()
                .map(|issue| issue.path.clone())
                .collect::<Vec<_>>(),
            vec!["Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv".to_string()],
            "a path starting at the season directory would name no series at all"
        );
    }

    /// A tree rsynced into itself directly under the root, with the run narrowed
    /// to that root: the duplication is reported, and the real content beside it
    /// is still assessed against the root it actually belongs to.
    #[tokio::test]
    async fn a_tree_nested_directly_into_itself_still_reports_the_duplication() {
        let temp_dir = TempDir::new().unwrap();
        let series = temp_dir.path().join("lib").join("Series");

        let nested = series
            .join("Series")
            .join("Veronica Mars")
            .join("Season 01");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("Veronica Mars - S01E01 - Pilot.mkv"), "").unwrap();

        let real = series.join("Elementary").join("Season 1");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("Elementary - S01E01 - Pilot.mkv"), "").unwrap();

        let report = ValidateCommand::new(series.clone())
            .execute()
            .await
            .unwrap();

        assert_eq!(report.library_root, temp_dir.path().join("lib"));
        assert_eq!(
            report.renames().next().map(|issue| issue.path.as_str()),
            Some("Series/Elementary/Season 1/Elementary - S01E01 - Pilot.mkv"),
            "the real content beside the duplication is still padded"
        );
        assert!(
            report.needing_decision().any(|issue| {
                issue.path.contains("Series/Series/")
                    && matches!(&issue.kind,
                    IssueKind::NeedsDecision { reason } if reason.contains("appears twice"))
            }),
            "the self-nested copy is a duplication, not a canonical library"
        );
    }

    #[tokio::test]
    async fn reported_issues_are_ordered_by_path() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        for name in ["Zulu", "Alpha", "Mike"] {
            let season = media_root.join(format!("Series/{name}/Season 1"));
            fs::create_dir_all(&season).unwrap();
            fs::write(season.join(format!("{name}.S01E01.Pilot.mkv")), "").unwrap();
        }

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        let paths: Vec<&str> = report
            .issues
            .iter()
            .map(|issue| issue.path.as_str())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();

        assert_eq!(paths, sorted, "a report is read top to bottom");
    }

    #[tokio::test]
    async fn an_episode_with_no_season_directory_is_given_one() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let series = media_root.join("Series/Loose Show");
        fs::create_dir_all(&series).unwrap();
        fs::write(series.join("Loose Show - S02E03 - Wandering.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(
            only_destination(&report),
            "Series/Loose Show/Season 02/Loose Show - S02E03 - Wandering.mkv"
        );
    }

    #[tokio::test]
    async fn an_episode_in_the_wrong_season_directory_is_moved_across() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let season = media_root.join("Series/Misfiled/Season 01");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Misfiled - S04E02 - Wrong Home.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.to_path_buf())
            .execute()
            .await
            .unwrap();

        assert_eq!(
            only_destination(&report),
            "Series/Misfiled/Season 04/Misfiled - S04E02 - Wrong Home.mkv",
            "the marker in the filename decides, not the directory it sat in"
        );
    }

    #[tokio::test]
    async fn scoping_to_one_series_reports_library_relative_paths() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let season = media_root.join("Series/Elementary/Season 6");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();

        // A second series that must not appear in a scoped run.
        let other = media_root.join("Series/Firefly/Season 1");
        fs::create_dir_all(&other).unwrap();
        fs::write(other.join("Firefly - s01e02 - The Train Job.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.join("Series/Elementary"))
            .execute()
            .await
            .unwrap();

        assert_eq!(report.scanned_files, 1, "only the scoped series is walked");
        assert_eq!(
            only_destination(&report),
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            "paths stay relative to the library root, not the directory scanned"
        );
        assert_eq!(report.library_root, media_root);
        assert_eq!(report.scan_path, media_root.join("Series/Elementary"));
    }

    #[tokio::test]
    async fn scoping_to_a_season_directory_works_the_same_way() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        let season = media_root.join("Anime/Cowboy Bebop/Season 1");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Cowboy Bebop.S01E05.Ballad.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.join("Anime/Cowboy Bebop/Season 1"))
            .execute()
            .await
            .unwrap();

        assert_eq!(
            only_destination(&report),
            "Anime/Cowboy Bebop/Season 01/Cowboy Bebop - S01E05 - Ballad.mkv"
        );
    }

    #[tokio::test]
    async fn a_scoped_run_still_honours_the_library_root_ignore_file() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        fs::write(
            media_root.join(".plexifyignore"),
            "*.tmp
Series/Elementary/
",
        )
        .unwrap();

        let season = media_root.join("Series/Elementary/Season 6");
        fs::create_dir_all(&season).unwrap();
        fs::write(season.join("Elementary - S06E08 Sand Trap.mkv"), "").unwrap();

        let report = ValidateCommand::new(media_root.join("Series/Elementary"))
            .execute()
            .await
            .unwrap();

        assert_eq!(
            report.scanned_files, 0,
            "the root said to skip this series; scoping into it must not override that"
        );
    }
}
