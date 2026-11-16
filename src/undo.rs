//! Putting a library back the way a fix run found it.
//!
//! An undo acts on a record written however long ago, so every reversal is
//! checked against the disk first and refused rather than guessed at. It writes
//! a record of its own, which makes it reversible in turn. See D23.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::fix::{
    absolute, carry_out, is_same_file, size_of, unix_seconds_now, FixOutcome, PlannedMove, Refusal,
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
/// Written to disk before any of it happens, in a different shape from the
/// outcome that replaces it: `moves` is an intention, `applied` is a fact.
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
    //
    // A media file and its sidecars went out together and come back together
    // or not at all. The record lists each media file ahead of its sidecars,
    // which is what lets the groups be read back out of it.
    let mut groups: Vec<Vec<PlannedMove>> = Vec::new();
    for applied in &record.applied {
        let reversal = PlannedMove {
            from: applied.to.clone(),
            to: applied.from.clone(),
            ..applied.clone()
        };
        match groups.last_mut() {
            Some(group) if applied.sidecar => group.push(reversal),
            _ => groups.push(vec![reversal]),
        }
    }
    groups.reverse();

    let mut moves = Vec::new();
    let mut refusals = Vec::new();

    for group in &groups {
        // Any member that cannot go back holds back the whole group. An
        // occupied path is refused even if another group would vacate it: no
        // fix moves a file onto a path another file leaves, and a group can be
        // refused after this one was planned, leaving the path taken.
        let blocked = group.iter().find_map(|reversal| {
            let source = absolute(media_root, &reversal.from);
            let destination = absolute(media_root, &reversal.to);

            let reason = match size_of(&source) {
                Err(reason) => reason,
                Ok(_) if destination.exists() && !is_same_file(&source, &destination) => {
                    RefusalReason::DestinationExists {
                        destination: reversal.to.clone(),
                    }
                }
                Ok(actual) => match reversal.size {
                    Some(expected) if expected == actual => return None,
                    Some(expected) => RefusalReason::ContentChanged {
                        expected_bytes: expected,
                        actual_bytes: actual,
                    },
                    None => RefusalReason::SizeUnrecorded,
                },
            };
            Some(Refusal {
                path: reversal.from.clone(),
                reason,
            })
        });

        match blocked {
            Some(refusal) => refusals.push(refusal),
            None => moves.extend(group.iter().cloned()),
        }
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
    carry_out(
        plan,
        &plan.media_root,
        &plan.moves,
        &plan.refusals,
        record_file,
    )
}

/// Where an undo run should write its own record by default.
pub fn default_record_file() -> PathBuf {
    PathBuf::from(format!("plexify-undo-{}.json", unix_seconds_now()))
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
    use crate::test_support::touch;
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
            notes: Vec::new(),
            library_root: root.to_path_buf(),
            scan_path: root.to_path_buf(),
            validation_time: Duration::from_secs(0),
        };

        let plan_file = root.join("fix-plan.json");
        let outcome = crate::fix::apply(&crate::fix::plan(&report), &plan_file).unwrap();
        (outcome, plan_file)
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
    fn a_blocked_subtitle_holds_back_its_video() {
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

        // A different subtitle has since taken the one the fix moved away.
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.en.srt",
            "other",
        );

        let outcome = apply(&plan(&record, &plan_file), &root.join("undo.json")).unwrap();

        assert!(
            outcome.applied.is_empty(),
            "putting the video back without its subtitle would split the pair: {:?}",
            outcome.applied
        );
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv").exists());
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.en.srt").exists());
    }

    #[test]
    fn a_blocked_video_holds_back_its_subtitle() {
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

        // Re-encoded in place since the fix.
        touch(
            root,
            "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            "a longer re-encode",
        );

        let outcome = apply(&plan(&record, &plan_file), &root.join("undo.json")).unwrap();

        assert!(outcome.applied.is_empty(), "{:?}", outcome.applied);
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.en.srt").exists());
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
    fn refuses_a_file_the_record_has_no_size_for() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(
            root,
            "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
            "video",
        );

        let (mut record, plan_file) = fix(
            root,
            &[(
                "Series/Show/Season 1/Show - S01E01 Pilot.mkv",
                "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            )],
        );
        record.applied[0].size = None;
        touch(
            root,
            "Series/Show/Season 01/Show - S01E01 - Pilot.mkv",
            "a much longer re-encode of the same episode",
        );

        let undo = plan(&record, &plan_file);

        assert!(
            undo.moves.is_empty(),
            "without a size there is no telling this is the file that was moved"
        );
        assert_eq!(undo.refusals[0].reason, RefusalReason::SizeUnrecorded);
    }

    /// Make a file impossible to stat while it stays where it is, as a
    /// permissions problem on a real library would.
    #[cfg(unix)]
    fn set_readable(file: &Path, readable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if readable { 0o755 } else { 0o600 };
        fs::set_permissions(file.parent().unwrap(), fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            fs::metadata(file).is_ok(),
            readable,
            "the permissions must bind this user, which they do not for root"
        );
    }

    #[cfg(windows)]
    fn set_readable(file: &Path, readable: bool) {
        // Neither denial stops a stat alone: a directory that can be listed
        // answers for its files' attributes.
        for (path, right) in [(file.parent().unwrap(), "(RD)"), (file, "(RA)")] {
            let mut icacls = std::process::Command::new("icacls");
            icacls.arg(path);
            if readable {
                icacls.args(["/remove:d", "*S-1-1-0"]);
            } else {
                icacls.args(["/deny", &format!("*S-1-1-0:{right}")]);
            }
            assert!(icacls.output().unwrap().status.success());
        }
        assert_eq!(fs::metadata(file).is_ok(), readable);
    }

    #[test]
    fn refuses_with_the_reason_when_the_file_cannot_be_read() {
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

        let moved = absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv");
        set_readable(&moved, false);
        let undo = plan(&record, &plan_file);
        set_readable(&moved, true);

        assert!(undo.moves.is_empty());
        assert!(
            matches!(undo.refusals[0].reason, RefusalReason::Unreadable { .. }),
            "the file is still there, so neither 'gone' nor '0 bytes' is true: {:?}",
            undo.refusals[0].reason
        );
    }

    #[test]
    fn a_path_a_refused_group_still_holds_is_refused_before_anything_moves() {
        let temp = TempDir::new().unwrap();
        let root = temp.path();
        touch(root, "Season 01/a.mkv", "video");
        touch(root, "Season 01/a.en.srt", "subs");
        touch(root, "Season 1/a.en.srt", "replaced since");

        // A record in which one move left a path that another then took. No fix
        // writes this - its plan refuses any destination that exists - so this
        // one is written by hand. The second file has changed since, so it stays
        // put, and the subtitle's original path stays taken.
        let moved = |from: &str, to: &str, sidecar: bool, size: u64| PlannedMove {
            from: from.to_string(),
            to: to.to_string(),
            sidecar,
            size: Some(size),
        };
        let record = FixOutcome {
            media_root: root.to_path_buf(),
            applied: vec![
                moved("Season 1/a.mkv", "Season 01/a.mkv", false, 5),
                moved("Season 1/a.en.srt", "Season 01/a.en.srt", true, 4),
                moved("Season 01/b.mkv", "Season 1/a.en.srt", false, 1),
            ],
            failed: Vec::new(),
            refusals: Vec::new(),
            emptied_directories: Vec::new(),
            plan_file: root.join("fix-plan.json"),
        };

        let undo = plan(&record, &record.plan_file);
        let outcome = apply(&undo, &root.join("undo.json")).unwrap();

        assert!(
            outcome.applied.is_empty() && outcome.failed.is_empty(),
            "the video must not go back without its subtitle: {outcome:?}"
        );
        assert!(absolute(root, "Season 01/a.mkv").exists());
        assert!(absolute(root, "Season 01/a.en.srt").exists());
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
        crate::fix::write_plan(&undo_plan, &record_file).unwrap();

        assert!(
            read_record(&record_file).is_err(),
            "an intention must not read back as a record of completed work"
        );

        let unfinished: UndoPlan =
            serde_json::from_str(&fs::read_to_string(&record_file).unwrap()).unwrap();
        assert_eq!(unfinished.moves.len(), 1, "it says what it meant to do");
    }

    #[test]
    fn writes_its_plan_before_putting_anything_back() {
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

        assert!(apply(&undo, &root.join("missing/dir/undo.json")).is_err());
        assert!(
            absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv").exists(),
            "nothing moves before the plan is on disk"
        );
    }

    #[test]
    fn never_overwrites_an_existing_record_file() {
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

        let record_file = root.join("undo.json");
        fs::write(&record_file, "an earlier run's record").unwrap();

        assert!(apply(&plan(&record, &plan_file), &record_file).is_err());
        assert_eq!(
            fs::read_to_string(&record_file).unwrap(),
            "an earlier run's record"
        );
        assert!(absolute(root, "Series/Show/Season 01/Show - S01E01 - Pilot.mkv").exists());
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
