//! Putting a library back the way a fix run found it.
//!
//! A fix writes down every move it made, which is enough to reverse it. What
//! makes undo harder than running a fix backwards is *when* it happens: a fix
//! acts on a report computed seconds earlier, while an undo acts on a record
//! written however long ago somebody took to notice something was wrong. The
//! library has had all that time to change underneath it.
//!
//! So every reversal is checked against the disk first, and each check refuses
//! rather than guesses:
//!
//! - The file is no longer where the fix put it.
//! - Something already occupies the path it came from - possibly a *different*
//!   file that has since been canonicalised into that name.
//! - The file is no longer the one that was moved, as far as its size can say.
//!
//! Size is weak evidence and is treated as such: it catches a file having been
//! replaced, not a re-encode that happens to be the same length. Hashing a media
//! library is not free, and the failure it would prevent - restoring a file to a
//! name it no longer suits - renames rather than destroys.
//!
//! An undo is itself a fix run in the other direction, so it writes its own
//! record. That is what makes it reversible in turn.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::fix::{
    absolute, emptied, is_same_file, rename, FailedMove, FixOutcome, PlannedMove, Refusal,
    RefusalReason,
};
use crate::paths::to_forward_slashes;

/// Read the record a fix run left behind.
pub fn read_record(plan_file: &Path) -> Result<FixOutcome> {
    let contents = fs::read_to_string(plan_file)
        .with_context(|| format!("could not read the plan file {plan_file:?}"))?;

    serde_json::from_str(&contents).with_context(|| {
        format!(
            "could not read {plan_file:?} as a record of a completed run - it may be from an older version of plexify, or from a run that was interrupted before it finished"
        )
    })
}

/// What reversing a fix run would do.
///
/// Written to disk before any of it happens, and deliberately a different shape
/// from the outcome that replaces it: `moves` is an intention, `applied` is a
/// fact. An undo interrupted halfway therefore leaves a file that says what it
/// meant to do, and cannot be mistaken for a record of what it did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoPlan {
    pub media_root: PathBuf,
    pub moves: Vec<PlannedMove>,
    pub refusals: Vec<Refusal>,
    /// The record being reversed.
    pub reversing: PathBuf,
}

/// Work out what can be put back, reading the filesystem but changing nothing.
pub fn plan(record: &FixOutcome, reversing: &Path) -> UndoPlan {
    let media_root = &record.media_root;

    // Only what the fix actually applied. A run that was interrupted has moves
    // it never made, and the record keeps the two apart precisely so an undo
    // does not try to reverse work that never happened.
    //
    // Reversed order, so a run is unwound in the order it was wound. Nothing in
    // a fix plan chains - a destination is canonical and a source never is, so
    // one file's destination is never another's source - but unwinding
    // backwards is the assumption-free way to do it.
    let reversals: Vec<PlannedMove> = record
        .applied
        .iter()
        .rev()
        .map(|applied| PlannedMove {
            from: applied.to.clone(),
            to: applied.from.clone(),
            sidecar: applied.sidecar,
            size: applied.size,
        })
        .collect();

    // A path this undo is about to vacate is not "occupied" for the purposes of
    // the check below: two files swapping back into place is the normal case
    // when a whole season was renamed.
    let vacating: HashSet<&str> = reversals.iter().map(|move_| move_.from.as_str()).collect();

    let mut moves = Vec::new();
    let mut refusals = Vec::new();

    for reversal in reversals.iter() {
        let source = absolute(media_root, &reversal.from);
        let destination = absolute(media_root, &reversal.to);

        if !source.exists() {
            refusals.push(Refusal {
                path: reversal.from.clone(),
                reason: RefusalReason::SourceMissing,
            });
            continue;
        }

        if destination.exists()
            && !is_same_file(&source, &destination)
            && !vacating.contains(reversal.to.as_str())
        {
            refusals.push(Refusal {
                path: reversal.from.clone(),
                reason: RefusalReason::DestinationExists {
                    destination: reversal.to.clone(),
                },
            });
            continue;
        }

        if let Some(expected) = reversal.size {
            let actual = fs::metadata(&source).map(|file| file.len()).unwrap_or(0);
            if actual != expected {
                refusals.push(Refusal {
                    path: reversal.from.clone(),
                    reason: RefusalReason::ContentChanged {
                        expected_bytes: expected,
                        actual_bytes: actual,
                    },
                });
                continue;
            }
        }

        moves.push(reversal.clone());
    }

    UndoPlan {
        media_root: media_root.clone(),
        moves,
        refusals,
        reversing: reversing.to_path_buf(),
    }
}

/// Put the files back, writing a record of this run before touching anything.
pub fn apply(plan: &UndoPlan, record_file: &Path) -> Result<FixOutcome> {
    // The intention first, in its own shape. Nothing here may be written into
    // an `applied` list before it has actually happened: a run that dies halfway
    // would then claim moves it never made, and undoing *that* record would try
    // to reverse work that was never done.
    write_plan(plan, record_file)?;

    let mut applied = Vec::new();
    let mut failed = Vec::new();
    let mut source_directories = Vec::new();

    for reversal in &plan.moves {
        let source = absolute(&plan.media_root, &reversal.from);
        let destination = absolute(&plan.media_root, &reversal.to);

        match rename(&source, &destination) {
            Ok(()) => {
                debug!("Put back {:?} -> {:?}", source, destination);
                if let Some(parent) = source.parent() {
                    if destination.parent() != Some(parent) {
                        source_directories.push(parent.to_path_buf());
                    }
                }
                applied.push(reversal.clone());
            }
            Err(error) => {
                warn!("Could not put back {:?}: {}", source, error);
                failed.push(FailedMove {
                    attempted: reversal.clone(),
                    error: format!("{error:#}"),
                });
            }
        }
    }

    let outcome = FixOutcome {
        media_root: plan.media_root.clone(),
        applied,
        failed,
        refusals: plan.refusals.clone(),
        emptied_directories: emptied(&plan.media_root, source_directories),
        plan_file: record_file.to_path_buf(),
    };
    write_record(&outcome, record_file)?;

    Ok(outcome)
}

/// Where an undo run should write its own record by default.
pub fn default_record_file() -> PathBuf {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default();

    PathBuf::from(format!("plexify-undo-{seconds}.json"))
}

fn write_plan(plan: &UndoPlan, record_file: &Path) -> Result<()> {
    let contents = serde_json::to_string_pretty(plan)?;
    fs::write(record_file, contents)
        .with_context(|| format!("could not write the undo plan to {record_file:?}"))
}

fn write_record(outcome: &FixOutcome, record_file: &Path) -> Result<()> {
    let contents = serde_json::to_string_pretty(outcome)?;
    fs::write(record_file, contents)
        .with_context(|| format!("could not write the undo record to {record_file:?}"))
}

/// Render what an undo would do, or did.
pub fn render(plan: &UndoPlan, outcome: Option<&FixOutcome>) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(out, "\n↩️  Undo");
    let _ = writeln!(out, "────────");
    let _ = writeln!(out, "📄 Reversing: {}", to_forward_slashes(&plan.reversing));
    let _ = writeln!(
        out,
        "📂 Library root: {}",
        to_forward_slashes(&plan.media_root)
    );

    match outcome {
        Some(outcome) => {
            let _ = writeln!(out, "✅ Put back: {}", outcome.applied.len());
            if !outcome.failed.is_empty() {
                let _ = writeln!(out, "❌ Failed: {}", outcome.failed.len());
            }
            let _ = writeln!(out, "📄 Record: {}", to_forward_slashes(&outcome.plan_file));
        }
        None => {
            let _ = writeln!(out, "↩️  Would put back: {}", plan.moves.len());
        }
    }

    if !plan.refusals.is_empty() {
        let _ = writeln!(out, "⛔ Refused: {}", plan.refusals.len());
    }

    if outcome.is_none() && !plan.moves.is_empty() {
        let _ = writeln!(out, "\n↩️  Would be put back:");
        let _ = writeln!(out, "─────────────────────");
        for reversal in &plan.moves {
            let _ = writeln!(out, "\n  {}", reversal.from);
            let _ = writeln!(out, "→ {}", reversal.to);
        }
    }

    if !plan.refusals.is_empty() {
        let _ = writeln!(out, "\n⛔ Refused, and left where they are:");
        let _ = writeln!(out, "───────────────────────────────────");
        for refusal in &plan.refusals {
            let _ = writeln!(out, "\n  {}", refusal.path);
            let _ = writeln!(out, "  {}", refusal.reason.explain());
        }
    }

    if let Some(outcome) = outcome {
        if !outcome.emptied_directories.is_empty() {
            let _ = writeln!(out, "\n📁 Left empty by this run, and not removed:");
            let _ = writeln!(out, "───────────────────────────────────────────");
            for directory in &outcome.emptied_directories {
                let _ = writeln!(out, "   {directory}");
            }
        }
    }

    if outcome.is_none() {
        let _ = writeln!(
            out,
            "\n   Nothing has been changed on disk. Re-run with --apply to put these back."
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::validate::{IssueKind, ValidationIssue, ValidationReport};
    use std::time::Duration;
    use tempfile::TempDir;

    /// Run a fix, then hand back what it recorded - the starting point for
    /// every test here, since an undo can only work from a real record.
    fn fix(root: &Path, moves: &[(&str, &str)]) -> (FixOutcome, PathBuf) {
        let report = ValidationReport {
            scanned_files: moves.len(),
            issues: moves
                .iter()
                .map(|(from, to)| ValidationIssue {
                    path: from.to_string(),
                    kind: IssueKind::Rename {
                        destination: to.to_string(),
                    },
                })
                .collect(),
            library_root: root.to_path_buf(),
            scan_path: root.to_path_buf(),
            validation_time: Duration::from_secs(0),
        };

        let plan_file = root.join("fix-plan.json");
        let outcome = crate::fix::apply(&crate::fix::plan(&report), &plan_file).unwrap();
        (outcome, plan_file)
    }

    fn touch(root: &Path, relative: &str, contents: &str) {
        let path = absolute(root, relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn puts_a_renamed_file_back_where_it_came_from() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv",
                "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv",
            )],
        );
        assert_eq!(record.applied.len(), 1);

        let outcome = apply(&plan(&record, &plan_file), &root.join("undo.json")).unwrap();

        assert_eq!(outcome.applied.len(), 1);
        assert!(absolute(
            root,
            "Series/Elementary/Season 6/Elementary - S06E08 Sand Trap.mkv"
        )
        .exists());
        assert!(!absolute(
            root,
            "Series/Elementary/Season 06/Elementary - S06E08 - Sand Trap.mkv"
        )
        .exists());
    }

    #[test]
    fn brings_sidecars_back_with_their_media_file() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.en.srt",
            "subs",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );
        assert_eq!(record.applied.len(), 2, "the fix moved both");

        apply(&plan(&record, &plan_file), &root.join("undo.json")).unwrap();

        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.mkv").exists());
        assert!(absolute(root, "Series/Show/Season 1/Show - S01E01 Pilot.en.srt").exists());
    }

    #[test]
    fn planning_alone_changes_nothing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let undo = plan(&record, &plan_file);

        assert_eq!(undo.moves.len(), 1);
        assert!(
            absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv").exists(),
            "the file is still where the fix left it"
        );
    }

    #[test]
    fn refuses_when_the_file_is_no_longer_where_the_fix_left_it() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        // Somebody moved it on somewhere else afterwards.
        fs::remove_file(absolute(
            root,
            "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
        ))
        .unwrap();

        let undo = plan(&record, &plan_file);

        assert!(undo.moves.is_empty());
        assert_eq!(undo.refusals[0].reason, RefusalReason::SourceMissing);
    }

    #[test]
    fn refuses_when_something_else_now_holds_the_original_name() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        // A different file has since taken the name the original had.
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "a different video",
        );

        let undo = plan(&record, &plan_file);

        assert!(undo.moves.is_empty(), "putting it back would overwrite");
        assert!(matches!(
            undo.refusals[0].reason,
            RefusalReason::DestinationExists { .. }
        ));
    }

    #[test]
    fn refuses_when_the_file_is_no_longer_the_one_that_was_moved() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        // Re-encoded in place: same name, different file.
        touch(
            root,
            "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            "a much longer re-encode of the same episode",
        );

        let undo = plan(&record, &plan_file);

        assert!(undo.moves.is_empty());
        assert!(
            matches!(
                undo.refusals[0].reason,
                RefusalReason::ContentChanged { .. }
            ),
            "got {:?}",
            undo.refusals[0].reason
        );
    }

    #[test]
    fn reverses_a_whole_season_that_swapped_places() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Series/Show/Season 1/Show - s01e01 - One.mkv", "a");
        touch(root, "Series/Show/Season 1/Show - s01e02 - Two.mkv", "bb");

        let (record, plan_file) = fix(
            root,
            &[
                (
                    "Series/Show/Season 1/Show - s01e01 - One.mkv",
                    "Series/Show/Season 01/Show - S01E01 - One.mkv",
                ),
                (
                    "Series/Show/Season 1/Show - s01e02 - Two.mkv",
                    "Series/Show/Season 01/Show - S01E02 - Two.mkv",
                ),
            ],
        );
        assert_eq!(record.applied.len(), 2);

        let outcome = apply(&plan(&record, &plan_file), &root.join("undo.json")).unwrap();

        assert_eq!(outcome.applied.len(), 2, "{:?}", outcome.refusals);
        assert!(absolute(root, "Series/Show/Season 1/Show - s01e01 - One.mkv").exists());
        assert!(absolute(root, "Series/Show/Season 1/Show - s01e02 - Two.mkv").exists());
    }

    #[test]
    fn an_undo_is_itself_reversible() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let undo_record_file = root.join("undo.json");
        let undone = apply(&plan(&record, &plan_file), &undo_record_file).unwrap();
        assert_eq!(undone.applied.len(), 1);

        // The undo wrote a record of its own, so it can be undone in turn.
        let reread = read_record(&undo_record_file).unwrap();
        let redone = apply(&plan(&reread, &undo_record_file), &root.join("redo.json")).unwrap();

        assert_eq!(redone.applied.len(), 1);
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv").exists());
    }

    #[test]
    fn an_interrupted_undo_leaves_a_record_that_claims_nothing() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let undo_plan = plan(&record, &plan_file);
        let record_file = root.join("undo.json");

        // What is on disk between the first write and the last one: an undo
        // that died here must not look like an undo that finished.
        write_plan(&undo_plan, &record_file).unwrap();

        assert!(
            read_record(&record_file).is_err(),
            "an intention must not read back as a record of completed work"
        );

        let unfinished: UndoPlan =
            serde_json::from_str(&fs::read_to_string(&record_file).unwrap()).unwrap();
        assert_eq!(unfinished.moves.len(), 1, "it says what it meant to do");
    }

    #[test]
    fn the_size_mismatch_reads_as_one_line() {
        let refusal = RefusalReason::ContentChanged {
            expected_bytes: 5,
            actual_bytes: 43,
        };

        assert_eq!(
            refusal.explain(),
            "this is no longer the file that was moved here: 5 bytes then, 43 now"
        );
    }
    #[test]
    fn reads_back_a_record_written_by_a_fix() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (_, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );

        let record = read_record(&plan_file).unwrap();

        assert_eq!(record.media_root, root);
        assert_eq!(record.applied.len(), 1);
        assert_eq!(
            record.applied[0].size,
            Some(5),
            "the size of what was moved is part of the record"
        );
    }

    #[test]
    fn refuses_a_file_that_is_not_a_record() {
        let temp = TempDir::new().unwrap();
        let not_a_plan = temp.path().join("notes.json");
        fs::write(&not_a_plan, "{\"hello\": \"world\"}").unwrap();

        let error = read_record(&not_a_plan).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains(
                "it may be from an older version of plexify, or from a run that was interrupted before it finished"
            ),
            "a parse failure is not proof the file is foreign, and the sentence saying so has to read as one: {message}"
        );
    }
}
