//! Applying the renames a validation report proposes.
//!
//! Validation says where each file belongs. This module moves it there, and its
//! whole design is about the gap between those two sentences: the report was
//! computed by reading paths, and by the time anything acts on it the library
//! may have moved on. Every proposal is therefore rechecked against the disk
//! immediately before it is applied.
//!
//! Three rules shape everything here, and all three exist because this runs
//! against a library nobody can reconstruct:
//!
//! - **A destination is never overwritten.** If something is already there, the
//!   move is refused and reported, never resolved by guessing.
//! - **Two files never claim one destination.** Canonicalising can map distinct
//!   sources onto the same name - two season directories that differ only by an
//!   arc name, say - and the right answer is a person's to give.
//! - **The plan is written before the first rename.** A run that dies halfway
//!   leaves a file on disk saying what it intended and how far it got.
//!
//! Nothing here constructs a destination. It only ever moves a file to the path
//! `naming::render` produced, which is what keeps the source string from
//! leaking into the result.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::commands::validate::{IssueKind, ValidationReport};

/// A rename that is going to be attempted, in library-relative form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedMove {
    pub from: String,
    pub to: String,
    /// Whether this is a file that belongs to a media file rather than the
    /// media file itself - a subtitle, an `.nfo`, artwork named after it.
    #[serde(default)]
    pub sidecar: bool,
    /// The size of the file when it was moved, recorded so that an undo can
    /// notice it is no longer the same file. Weak evidence - a re-encode of the
    /// same length would pass - but free, and it catches the ordinary case of a
    /// file having been replaced since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// A proposal that will not be attempted, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    pub path: String,
    pub reason: RefusalReason,
}

/// Why a proposed rename was not attempted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefusalReason {
    /// Another file already occupies the destination.
    DestinationExists { destination: String },
    /// Another source in this same run canonicalises to the destination.
    DestinationClaimedTwice { destination: String },
    /// The file named by the report is no longer where it was.
    SourceMissing,
    /// The file is no longer the one that was moved there.
    ContentChanged {
        expected_bytes: u64,
        actual_bytes: u64,
    },
}

impl RefusalReason {
    /// A one-line explanation, for the report.
    pub fn explain(&self) -> String {
        match self {
            RefusalReason::DestinationExists { destination } => {
                format!("'{destination}' already exists; it will not be overwritten")
            }
            RefusalReason::DestinationClaimedTwice { destination } => {
                format!("more than one file canonicalises to '{destination}'")
            }
            RefusalReason::SourceMissing => {
                "the file is no longer at the path that was reported".to_string()
            }
            RefusalReason::ContentChanged {
                expected_bytes,
                actual_bytes,
            } => format!(
                "this is no longer the file that was moved here: {expected_bytes} bytes then, {actual_bytes} now"
            ),
        }
    }
}

/// What a fix run intends to do, written to disk before it does any of it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixPlan {
    pub media_root: PathBuf,
    pub created_unix_seconds: u64,
    pub moves: Vec<PlannedMove>,
    pub refusals: Vec<Refusal>,
}

/// What a fix run actually did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixOutcome {
    /// The root every path here is relative to. Without it the record is not
    /// enough to act on, which is what an undo needs it for.
    pub media_root: PathBuf,
    pub applied: Vec<PlannedMove>,
    pub failed: Vec<FailedMove>,
    pub refusals: Vec<Refusal>,
    /// Directories the run emptied. They are reported, never removed.
    pub emptied_directories: Vec<String>,
    pub plan_file: PathBuf,
}

/// A rename that was attempted and did not succeed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailedMove {
    #[serde(flatten)]
    pub attempted: PlannedMove,
    pub error: String,
}

/// Decide what to do with the renames a report proposes.
///
/// Reads the filesystem but changes nothing.
pub fn plan(report: &ValidationReport) -> FixPlan {
    // The library root, not the directory that was walked. A run narrowed to one
    // season still proposes destinations relative to the root - and a file's
    // destination is often outside the subtree that was scanned, since correcting
    // `Season 6` to `Season 06` moves it into a sibling directory.
    let media_root = &report.library_root;

    // A media file and the files named after it move together or not at all.
    // Renaming a `.webm` and leaving its `.vtt` behind would break the pairing
    // that `work` relies on, and would do it silently.
    let groups: Vec<Vec<PlannedMove>> = report
        .renames()
        .filter_map(|issue| match &issue.kind {
            IssueKind::Rename { destination } => {
                let media = PlannedMove {
                    from: issue.path.clone(),
                    to: destination.clone(),
                    sidecar: false,
                    size: None,
                };
                let mut group = vec![media];
                group.extend(sidecars_of(media_root, &issue.path, destination));
                Some(group)
            }
            IssueKind::NeedsDecision { .. } => None,
        })
        .collect();

    // A destination that more than one file resolves to cannot be handed to
    // either of them: whichever moved first would be overwritten by the second.
    let mut claims: HashMap<&str, usize> = HashMap::new();
    for planned in groups.iter().flatten() {
        *claims.entry(planned.to.as_str()).or_default() += 1;
    }

    let mut moves = Vec::new();
    let mut refusals = Vec::new();

    for group in &groups {
        let media = &group[0];

        // Any member that cannot move stops the whole group, so a file never
        // ends up separated from the files that belong to it.
        let blocked = group.iter().find_map(|planned| {
            let source = absolute(media_root, &planned.from);
            let destination = absolute(media_root, &planned.to);

            if claims.get(planned.to.as_str()).copied().unwrap_or(0) > 1 {
                return Some(RefusalReason::DestinationClaimedTwice {
                    destination: planned.to.clone(),
                });
            }
            if !source.exists() {
                return Some(RefusalReason::SourceMissing);
            }
            if destination.exists() && !is_same_file(&source, &destination) {
                return Some(RefusalReason::DestinationExists {
                    destination: planned.to.clone(),
                });
            }
            None
        });

        match blocked {
            Some(reason) => refusals.push(Refusal {
                path: media.from.clone(),
                reason,
            }),
            None => moves.extend(group.iter().cloned()),
        }
    }

    FixPlan {
        media_root: media_root.clone(),
        created_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or_default(),
        moves,
        refusals,
    }
}

/// Carry out a plan, writing it to `plan_file` before touching anything.
pub fn apply(plan: &FixPlan, plan_file: &Path) -> Result<FixOutcome> {
    write_plan(plan, plan_file)?;

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    let mut source_directories = Vec::new();

    for planned in &plan.moves {
        let source = absolute(&plan.media_root, &planned.from);
        let destination = absolute(&plan.media_root, &planned.to);

        match rename(&source, &destination) {
            Ok(()) => {
                debug!("Renamed {:?} -> {:?}", source, destination);
                if let Some(parent) = source.parent() {
                    if destination.parent() != Some(parent) {
                        source_directories.push(parent.to_path_buf());
                    }
                }
                applied.push(PlannedMove {
                    size: fs::metadata(&destination).ok().map(|file| file.len()),
                    ..planned.clone()
                });
            }
            Err(error) => {
                warn!("Failed to rename {:?}: {}", source, error);
                failed.push(FailedMove {
                    attempted: planned.clone(),
                    error: format!("{error:#}"),
                });
            }
        }
    }

    let emptied_directories = emptied(&plan.media_root, source_directories);

    let outcome = FixOutcome {
        media_root: plan.media_root.clone(),
        applied,
        failed,
        refusals: plan.refusals.clone(),
        emptied_directories,
        plan_file: plan_file.to_path_buf(),
    };

    // Record what happened alongside what was intended, so an interrupted run
    // and a completed one are told apart by reading one file.
    write_outcome(&outcome, plan_file)?;

    Ok(outcome)
}

/// Where a fix run should write its plan by default.
///
/// The current directory rather than the library: a record of what was done to
/// the library is not itself part of the library.
pub fn default_plan_file(created_unix_seconds: u64) -> PathBuf {
    PathBuf::from(format!("plexify-fix-{created_unix_seconds}.json"))
}

pub(crate) fn rename(source: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create directory {parent:?}"))?;
    }

    fs::rename(source, destination)
        .with_context(|| format!("could not move {source:?} to {destination:?}"))
}

fn write_plan(plan: &FixPlan, plan_file: &Path) -> Result<()> {
    let contents = serde_json::to_string_pretty(plan)?;
    fs::write(plan_file, contents)
        .with_context(|| format!("could not write the plan to {plan_file:?}"))
}

fn write_outcome(outcome: &FixOutcome, plan_file: &Path) -> Result<()> {
    let contents = serde_json::to_string_pretty(outcome)?;
    fs::write(plan_file, contents)
        .with_context(|| format!("could not write the outcome to {plan_file:?}"))
}

/// The files that belong to a media file, and where they go when it moves.
///
/// A sidecar is a sibling named after the media file: `X.vtt`, `X.en.srt`,
/// `X.nfo`. Anything whose own extension is a media extension is excluded - two
/// videos sharing a stem are two videos, each with its own destination, and
/// dragging one along with the other would be wrong.
fn sidecars_of(media_root: &Path, from: &str, to: &str) -> Vec<PlannedMove> {
    let source = absolute(media_root, from);
    let (Some(directory), Some(source_name)) = (source.parent(), file_name_of(from)) else {
        return Vec::new();
    };
    let (Some(source_stem), Some(destination_name)) = (stem_of(source_name), file_name_of(to))
    else {
        return Vec::new();
    };
    let Some(destination_stem) = stem_of(destination_name) else {
        return Vec::new();
    };

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut sidecars: Vec<PlannedMove> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name == source_name {
                return None;
            }

            let suffix = name.strip_prefix(source_stem)?.strip_prefix('.')?;
            if suffix.is_empty() || is_media_extension(suffix) {
                return None;
            }

            Some(PlannedMove {
                from: rejoin(parent_of(from), &name),
                to: rejoin(parent_of(to), &format!("{destination_stem}.{suffix}")),
                sidecar: true,
                size: None,
            })
        })
        .collect();

    sidecars.sort_by(|left, right| left.from.cmp(&right.from));
    sidecars
}

/// Whether a suffix ends in an extension that makes the file a media file.
fn is_media_extension(suffix: &str) -> bool {
    let extension = suffix.rsplit('.').next().unwrap_or(suffix);
    crate::commands::validate::MEDIA_EXTENSIONS
        .iter()
        .any(|known| extension.eq_ignore_ascii_case(known))
}

fn parent_of(relative: &str) -> &str {
    match relative.rsplit_once('/') {
        Some((parent, _)) => parent,
        None => "",
    }
}

fn file_name_of(relative: &str) -> Option<&str> {
    let name = relative.rsplit('/').next()?;
    (!name.is_empty()).then_some(name)
}

fn stem_of(name: &str) -> Option<&str> {
    let (stem, _) = name.rsplit_once('.')?;
    (!stem.is_empty()).then_some(stem)
}

fn rejoin(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Join a library-relative, forward-slash path onto the media root.
///
/// Split on `/` rather than handing the whole string to `join`, so the result
/// carries native separators on every platform.
pub(crate) fn absolute(media_root: &Path, relative: &str) -> PathBuf {
    let mut path = media_root.to_path_buf();
    for component in relative.split('/').filter(|part| !part.is_empty()) {
        path.push(component);
    }
    path
}

/// Whether two paths name the same file on disk.
///
/// This is what separates a collision from a change of case. On a
/// case-insensitive filesystem `.../show - s01e01.mkv` and `.../show - S01E01.mkv`
/// both exist and both are the file being renamed, so treating "destination
/// exists" as a collision would refuse every correction of a lowercase marker -
/// the single most common fix in a real library.
pub(crate) fn is_same_file(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// Which of the directories a run moved files out of are now empty.
pub(crate) fn emptied(media_root: &Path, mut directories: Vec<PathBuf>) -> Vec<String> {
    directories.sort();
    directories.dedup();

    let mut emptied: Vec<String> = directories
        .into_iter()
        .filter(|directory| is_empty(directory))
        .filter_map(|directory| {
            directory
                .strip_prefix(media_root)
                .ok()
                .map(crate::paths::to_forward_slashes)
        })
        .collect();

    emptied.sort();
    emptied
}

fn is_empty(directory: &Path) -> bool {
    match fs::read_dir(directory) {
        Ok(mut entries) => entries.next().is_none(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::validate::ValidationIssue;
    use std::time::Duration;
    use tempfile::TempDir;

    fn report(root: &Path, issues: Vec<ValidationIssue>) -> ValidationReport {
        ValidationReport {
            scanned_files: issues.len(),
            issues,
            library_root: root.to_path_buf(),
            scan_path: root.to_path_buf(),
            validation_time: Duration::from_secs(0),
        }
    }

    fn rename_issue(from: &str, to: &str) -> ValidationIssue {
        ValidationIssue {
            path: from.to_string(),
            kind: IssueKind::Rename {
                destination: to.to_string(),
            },
        }
    }

    fn touch(root: &Path, relative: &str) {
        let path = absolute(root, relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn moves_a_file_to_its_canonical_path() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
        );

        let report = report(
            root,
            vec![rename_issue(
                "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
                "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert_eq!(outcome.applied.len(), 1);
        assert!(absolute(
            root,
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
        )
        .exists());
        assert!(!absolute(
            root,
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv"
        )
        .exists());
    }

    #[test]
    fn refuses_to_overwrite_a_file_that_is_already_there() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");
        touch(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(outcome.applied.is_empty());
        assert_eq!(
            outcome.refusals,
            vec![Refusal {
                path: "Series/Show/Season 1/Show - S01E01 Pilot.mkv".to_string(),
                reason: RefusalReason::DestinationExists {
                    destination: "Series/Show/Season 01/Show - S01E01 - Pilot.mkv".to_string()
                }
            }]
        );
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv").exists());
    }

    #[test]
    fn refuses_a_destination_two_files_both_want() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 01 - The Arc/Show - S01E01 - Pilot.mkv",
        );
        touch(root, "Series/Show/Season 1/Show - S01E01 - Pilot.mkv");

        let destination = "Series/Show/Season 01/Show - S01E01 - Pilot.mkv";
        let report = report(
            root,
            vec![
                rename_issue(
                    "Series/Show/Season 01 - The Arc/Show - S01E01 - Pilot.mkv",
                    destination,
                ),
                rename_issue(
                    "Series/Show/Season 1/Show - S01E01 - Pilot.mkv",
                    destination,
                ),
            ],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(outcome.applied.is_empty(), "neither file may move");
        assert_eq!(outcome.refusals.len(), 2);
        assert!(outcome.refusals.iter().all(|refusal| matches!(
            refusal.reason,
            RefusalReason::DestinationClaimedTwice { .. }
        )));
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 - Pilot.mkv").exists());
    }

    #[test]
    fn corrects_a_marker_that_differs_only_in_case() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 01/Show - s01e01 - Pilot.mkv");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 01/Show - s01e01 - Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert_eq!(
            outcome.applied.len(),
            1,
            "a case-only rename is the same file, not a collision: {:?}",
            outcome.refusals
        );

        let season = absolute(root, "Series/Show/Season 01");
        let names: Vec<String> = fs::read_dir(season)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["Show - S01E01 - Pilot.mkv".to_string()]);
    }

    #[test]
    fn refuses_a_file_that_has_moved_since_the_report() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/gone.mkv",
                "Series/Show/Season 01/Show - S01E01.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(outcome.applied.is_empty());
        assert_eq!(outcome.refusals[0].reason, RefusalReason::SourceMissing);
    }

    #[test]
    fn reports_a_directory_it_emptied_without_removing_it() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert_eq!(outcome.emptied_directories, vec!["Series/Show/Season 1"]);
        assert!(
            absolute(root, "Series/Show/Season 1").exists(),
            "an emptied directory is reported, not removed"
        );
    }

    #[test]
    fn leaves_a_directory_off_the_emptied_list_when_something_remains() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");
        touch(root, "Series/Show/Season 1/artwork.tmp");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(outcome.emptied_directories.is_empty());
    }

    #[test]
    fn writes_the_plan_before_moving_anything() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );
        let plan = plan(&report);
        let plan_file = temp.path().join("plan.json");

        // The plan on its own describes the work without doing any of it.
        write_plan(&plan, &plan_file).unwrap();
        assert!(plan_file.exists());
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv").exists());

        let written: FixPlan =
            serde_json::from_str(&fs::read_to_string(&plan_file).unwrap()).unwrap();
        assert_eq!(written.moves, plan.moves);

        let outcome = apply(&plan, &plan_file).unwrap();
        let recorded: FixOutcome =
            serde_json::from_str(&fs::read_to_string(&plan_file).unwrap()).unwrap();
        assert_eq!(recorded.applied, outcome.applied);
    }

    #[test]
    fn planning_alone_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let plan = plan(&report);

        assert_eq!(plan.moves.len(), 1);
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv").exists());
        assert!(!absolute(root, "Series/Show/Season 01").exists());
    }

    #[test]
    fn a_subtitle_follows_the_video_it_belongs_to() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        let show = "Series/Super Best Friends Play - FFX";
        touch(root, &format!("{show}/Play - S01E13 (1080p60).webm"));
        touch(root, &format!("{show}/Play - S01E13 (1080p60).vtt"));

        let report = report(
            root,
            vec![rename_issue(
                &format!("{show}/Play - S01E13 (1080p60).webm"),
                &format!("{show}/Play - S01E13 [1080p60].webm"),
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert_eq!(outcome.applied.len(), 2, "the video and its subtitle");
        assert!(absolute(root, &format!("{show}/Play - S01E13 [1080p60].vtt")).exists());
        assert!(!absolute(root, &format!("{show}/Play - S01E13 (1080p60).vtt")).exists());
        assert!(outcome.applied[1].sidecar);
    }

    #[test]
    fn a_language_tagged_subtitle_keeps_its_tag() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.en.srt");
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.nfo");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.en.srt").exists());
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.nfo").exists());
    }

    #[test]
    fn another_video_sharing_a_stem_is_not_dragged_along() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mp4");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert_eq!(outcome.applied.len(), 1, "the mp4 has its own destination");
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mp4").exists());
    }

    #[test]
    fn a_blocked_subtitle_holds_back_the_video_too() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv");
        touch(root, "Series/Show/Season 1/Show - S01E01 Pilot.srt");
        // Something already occupies where the subtitle would go.
        touch(root, "Series/Show/Season 01/Show - S01E01 - Pilot.srt");

        let report = report(
            root,
            vec![rename_issue(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let outcome = apply(&plan(&report), &temp.path().join("plan.json")).unwrap();

        assert!(
            outcome.applied.is_empty(),
            "moving the video without its subtitle would split the pair"
        );
        assert_eq!(outcome.refusals.len(), 1);
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv").exists());
    }

    #[test]
    fn a_scoped_run_moves_files_out_of_the_subtree_that_was_scanned() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
        );

        // What `validate` produces when pointed at the season directory: paths
        // relative to the library root, but only that subtree walked.
        let scoped = ValidationReport {
            scanned_files: 1,
            issues: vec![rename_issue(
                "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
                "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            )],
            library_root: root.to_path_buf(),
            scan_path: root.join("Series/Elementary/Season 6"),
            validation_time: Duration::from_secs(0),
        };

        let outcome = apply(&plan(&scoped), &temp.path().join("plan.json")).unwrap();

        assert_eq!(
            outcome.applied.len(),
            1,
            "destinations resolve from the library root, not the scanned directory: {:?}",
            outcome.refusals
        );
        assert!(absolute(
            root,
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
        )
        .exists());
    }
}
