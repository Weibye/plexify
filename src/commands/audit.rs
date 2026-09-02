//! Report what each file in a library needs before a client will Direct Play
//! it. Read-only: it proposes nothing and changes nothing.

use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::commands::validate::MEDIA_EXTENSIONS;
use crate::ignore::IgnoreFilter;
use crate::paths::to_forward_slashes;
use crate::probe::probe;
use crate::queue::QueueDirectory;
use crate::target::{evaluate, Conformance, Cost, Finding, PlaybackTarget, Provenance};

/// One file's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The file relative to the scanned directory, with `/` separators.
    pub path: String,
    pub size_bytes: u64,
    /// How much of `size_bytes` is actually backed by disk. See [`Allocation`].
    pub allocation: Allocation,
    /// When the file was last written, out of the same `stat`.
    ///
    /// A shortfall cannot tell a lost file from one still being copied, and no
    /// threshold can: a preallocated file mid-transfer is exactly what a sparse
    /// file looks like. This is what lets the report attach that caveat to the
    /// individual line rather than only to the heading above the list.
    pub modified: Option<SystemTime>,
    pub outcome: Outcome,
}

/// How much of a file's apparent length is really on the disk.
///
/// A sparse file reports its holes as zeros, so FFprobe reads one happily and
/// the conformance verdict above it is drawn from a header describing content
/// that is partly absent. Comparing the two sizes catches that, and it is the
/// only shape of incompleteness that costs nothing to look for: it is one more
/// field of the `stat` the audit already performs, and no bytes are read.
///
/// Nothing acts on it. A file preallocated by a copy that is still running is
/// indistinguishable from one whose content was lost, so this reports a
/// measurement - how much is not on disk - and never a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Allocation {
    /// `st_blocks * 512`. The unit is 512 bytes by definition of the field,
    /// whatever block size the filesystem underneath actually uses.
    Measured { allocated_bytes: u64 },
    /// The platform exposes no allocated size, so nothing is claimed.
    ///
    /// Windows has no `st_blocks`, and `std`'s Windows `MetadataExt` exposes no
    /// equivalent; the allocated size is only reachable through
    /// `GetCompressedFileSize` or `FSCTL_QUERY_ALLOCATED_RANGES`, neither of
    /// which this project links. Reporting "0% missing" there would be the
    /// worst of the three options, because it reads as a clean bill of health
    /// for a check that was never run.
    Unmeasurable,
}

/// The smallest shortfall worth reporting.
///
/// Allocation accounting is under-inclusive for reasons that have nothing to do
/// with holes: ext4 and btrfs store a small enough file inside its inode and
/// charge it no blocks at all, and a filesystem allocating in large units can
/// disagree with the apparent size by up to one of them (a megabyte, at ZFS's
/// largest record size). One megabyte covers both, and against media files -
/// which run to hundreds of megabytes - it discards nothing anyone would act on.
const SHORTFALL_FLOOR_BYTES: u64 = 1 << 20;

/// The smallest share of a file worth reporting, as a percentage.
///
/// The floor above does not cover transparent compression, which is unbounded
/// and makes allocated legitimately smaller than apparent with no hole present.
/// The number is set by the gap between the two things it has to separate. A
/// media file's payload is already compressed, so btrfs, ZFS and NTFS recover
/// low single-digit percentages on one at best and mostly give up on the
/// extents entirely; the sparse files measured on the real library were missing
/// between 35% and 76% of themselves. Five percent sits clear of the first and
/// far below the second.
const SHORTFALL_PERCENT: u64 = 5;

impl Allocation {
    /// The bytes of `apparent_bytes` that no allocation backs, when that is
    /// large enough to mean something. `None` covers a whole file, a file whose
    /// shortfall is within the tolerances above, and a platform that cannot
    /// measure - callers must not read `None` as "this file is whole".
    pub fn shortfall(self, apparent_bytes: u64) -> Option<u64> {
        let Allocation::Measured { allocated_bytes } = self else {
            return None;
        };

        let missing = apparent_bytes.saturating_sub(allocated_bytes);
        (missing >= SHORTFALL_FLOOR_BYTES && missing * 100 >= apparent_bytes * SHORTFALL_PERCENT)
            .then_some(missing)
    }
}

/// Whether this platform can read an allocated size at all.
///
/// It separates the two things [`Allocation::Unmeasurable`] otherwise conflates:
/// where this is false nothing was measured and the report says so once, and
/// where it is true an `Unmeasurable` entry is one file whose own `stat` failed,
/// which has to be named rather than left looking whole.
pub const ALLOCATION_IS_MEASURABLE: bool = cfg!(unix);

/// Read the allocated size out of a `stat` that has already been performed.
#[cfg(unix)]
fn allocation_of(metadata: &std::fs::Metadata) -> Allocation {
    use std::os::unix::fs::MetadataExt;

    Allocation::Measured {
        allocated_bytes: metadata.blocks().saturating_mul(512),
    }
}

#[cfg(not(unix))]
fn allocation_of(_metadata: &std::fs::Metadata) -> Allocation {
    Allocation::Unmeasurable
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Assessed(Conformance),
    /// FFprobe could not read the file, so nothing is claimed about it.
    ProbeFailed {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub target: String,
    pub scan_path: PathBuf,
    pub entries: Vec<AuditEntry>,
    pub audit_time: Duration,
}

impl AuditReport {
    /// The files in one cost bucket. `None` is the bucket that needs nothing.
    pub fn bucket(&self, cost: Option<Cost>) -> impl Iterator<Item = &AuditEntry> {
        self.entries
            .iter()
            .filter(move |entry| match &entry.outcome {
                Outcome::Assessed(conformance) => conformance.cost() == cost,
                Outcome::ProbeFailed { .. } => false,
            })
    }

    pub fn unreadable(&self) -> impl Iterator<Item = &AuditEntry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.outcome, Outcome::ProbeFailed { .. }))
    }

    /// Files holding measurably less than they claim, with the missing bytes.
    ///
    /// This cuts across the cost buckets rather than joining them: an
    /// incomplete file still has whatever conformance its header describes, and
    /// this says nothing about that.
    pub fn incomplete(&self) -> impl Iterator<Item = (&AuditEntry, u64)> {
        self.entries.iter().filter_map(|entry| {
            entry
                .allocation
                .shortfall(entry.size_bytes)
                .map(|missing| (entry, missing))
        })
    }

    /// Files this platform could have measured but did not, because their own
    /// `stat` failed. Always zero where the platform cannot measure at all.
    pub fn unmeasured(&self) -> impl Iterator<Item = &AuditEntry> {
        self.entries.iter().filter(|entry| {
            ALLOCATION_IS_MEASURABLE && matches!(entry.allocation, Allocation::Unmeasurable)
        })
    }
}

pub struct AuditCommand {
    scan_path: PathBuf,
    target: PlaybackTarget,
}

impl AuditCommand {
    /// `target` is a built-in envelope's name or a path to a TOML one.
    pub fn new(path: PathBuf, target: &str) -> Result<Self> {
        let scan_path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        };

        Ok(Self {
            scan_path,
            target: PlaybackTarget::load(target)?,
        })
    }

    pub async fn execute(&self) -> Result<AuditReport> {
        let started = Instant::now();

        if !self.scan_path.is_dir() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.scan_path
            ));
        }

        info!(
            "🔍 Auditing {:?} against {}",
            self.scan_path, self.target.name
        );

        let files = self.media_files();
        info!("🔍 Found {} media files, probing...", files.len());

        let progress = ProgressBar::new(files.len() as u64);
        progress.set_style(
            ProgressStyle::with_template("Probing {bar:30.cyan/blue} {pos}/{len} {msg}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );
        progress.set_message("files");

        let mut entries: Vec<AuditEntry> = files
            .par_iter()
            .map(|path| {
                let entry = self.audit_file(path);
                progress.inc(1);
                entry
            })
            .collect();
        progress.finish_and_clear();

        // Rayon returns work in whatever order it finished; a report is read
        // top to bottom, so order it the way the library is laid out.
        entries.sort_by(|left, right| left.path.cmp(&right.path));

        let report = AuditReport {
            target: self.target.name.clone(),
            scan_path: self.scan_path.clone(),
            entries,
            audit_time: started.elapsed(),
        };

        info!(
            "✅ Audit complete. {} files in {:.2}s",
            report.entries.len(),
            report.audit_time.as_secs_f64()
        );

        Ok(report)
    }

    fn audit_file(&self, path: &Path) -> AuditEntry {
        let relative = path.strip_prefix(&self.scan_path).unwrap_or(path);
        let outcome = match probe(path) {
            Ok(media) => Outcome::Assessed(evaluate(&media, &self.target)),
            Err(error) => Outcome::ProbeFailed {
                reason: format!("{error:#}"),
            },
        };

        // One stat, read twice: the length the file claims, and how much of it
        // the filesystem is actually holding.
        let metadata = std::fs::metadata(path).ok();

        AuditEntry {
            path: to_forward_slashes(relative),
            size_bytes: metadata.as_ref().map(|m| m.len()).unwrap_or(0),
            allocation: metadata
                .as_ref()
                .map(allocation_of)
                .unwrap_or(Allocation::Unmeasurable),
            modified: metadata.as_ref().and_then(|m| m.modified().ok()),
            outcome,
        }
    }

    fn media_files(&self) -> Vec<PathBuf> {
        let filter = match IgnoreFilter::new(self.scan_path.clone()) {
            Ok(filter) => Some(filter),
            Err(e) => {
                warn!("Failed to load .plexifyignore patterns: {}", e);
                None
            }
        };

        WalkDir::new(&self.scan_path)
            .follow_links(false)
            .into_iter()
            // Prune ignored subtrees rather than walking and discarding them.
            .filter_entry(|entry| {
                let path = entry.path();
                if path == self.scan_path || !path.is_dir() {
                    return true;
                }

                // A work root inside the media root is the mistake people make
                // constantly, and its half-written outputs are not library
                // files. Auditing them reports the queue's own scratch space.
                if is_queue_directory(path) {
                    debug!("🚫 Skipping queue directory: {:?}", path);
                    return false;
                }

                match &filter {
                    Some(filter) if filter.should_skip_dir(path) => {
                        debug!("🚫 Skipping entire directory: {:?}", path);
                        false
                    }
                    _ => true,
                }
            })
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.into_path())
            .filter(|path| {
                !path.is_dir()
                    && filter.as_ref().is_none_or(|f| !f.should_ignore(path))
                    && path
                        .extension()
                        .map(|ext| {
                            MEDIA_EXTENSIONS
                                .contains(&ext.to_string_lossy().to_lowercase().as_str())
                        })
                        .unwrap_or(false)
            })
            .collect()
    }

    pub fn print_report(&self, report: &AuditReport) {
        print!("{}", self.render_report(report));
    }

    /// Render the audit.
    ///
    /// Buckets come first and are never merged: a remux copies the video
    /// bitstream and finishes in minutes, a re-encode on a Pi 4 runs slower
    /// than realtime, and one total covering both says nothing useful.
    pub fn render_report(&self, report: &AuditReport) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        let _ = writeln!(out, "\n📊 Direct Play Audit");
        let _ = writeln!(out, "════════════════════");
        let _ = writeln!(out, "🎯 Target: {}", report.target);
        if let Some(device) = &self.target.device {
            let _ = writeln!(out, "📺 Device: {device}");
        }
        let _ = writeln!(
            out,
            "📂 Scanned directory: {}",
            to_forward_slashes(&report.scan_path)
        );
        let _ = writeln!(out, "📁 Files probed: {}", report.entries.len());
        let _ = writeln!(
            out,
            "⏱️  Audit time: {:.2}s",
            report.audit_time.as_secs_f64()
        );

        if report.entries.is_empty() {
            let _ = writeln!(out, "\nNo media files found.");
            return out;
        }

        let _ = writeln!(out, "\n💰 By cost:");
        let _ = writeln!(out, "───────────");
        for (cost, label) in [
            (None, "✅ Direct Plays as it is"),
            (
                Some(Cost::Remux),
                "🔁 Remux - audio or subtitles, video copied",
            ),
            (Some(Cost::Reencode), "🔥 Re-encode - the video itself"),
        ] {
            let files: Vec<_> = report.bucket(cost).collect();
            let bytes: u64 = files.iter().map(|entry| entry.size_bytes).sum();
            let _ = writeln!(
                out,
                "   {:5} ({:5.1}%)  {:8.1} GB  {label}",
                files.len(),
                percent(files.len(), report.entries.len()),
                bytes as f64 / 1e9,
            );
        }

        let unreadable = report.unreadable().count();
        if unreadable > 0 {
            let _ = writeln!(
                out,
                "   {unreadable:5}             {:8}      ❓ Could not be probed, so nothing is claimed",
                ""
            );
        }

        // Deliberately its own section rather than a bucket or a finding: an
        // incomplete file keeps whatever verdict its header earned, and this
        // neither changes it nor proposes anything be done about it.
        if !ALLOCATION_IS_MEASURABLE {
            if !report.entries.is_empty() {
                let _ = writeln!(out, "\n🕳️  How much of each file is on the disk:");
                let _ = writeln!(out, "─────────────────────────────────────────");
                let _ = writeln!(
                    out,
                    "   Not measurable on this platform, so no file below was checked.\n   Allocated size comes from stat's st_blocks, which Windows does not have."
                );
            }
        } else {
            let mut missing: Vec<_> = report.incomplete().collect();
            if !missing.is_empty() {
                missing.sort_by(|(left, l), (right, r)| {
                    (r * 100 / right.size_bytes.max(1))
                        .cmp(&(l * 100 / left.size_bytes.max(1)))
                        .then(left.path.cmp(&right.path))
                });

                let total: u64 = missing.iter().map(|(_, bytes)| bytes).sum();
                let _ = writeln!(out, "\n🕳️  Content that is not on the disk:");
                let _ = writeln!(out, "────────────────────────────────────");
                let _ = writeln!(
                    out,
                    "   {} file(s) are shorter on disk than they claim to be - {:.1} GB in all.\n   FFprobe reads the absent parts as zeros, so the verdicts above do not see this.\n   A file still being written looks the same, so nothing here is treated as damage.",
                    missing.len(),
                    total as f64 / 1e9,
                );
                for (entry, bytes) in missing {
                    let _ = writeln!(
                        out,
                        "   {:3.0}% of this file is not on disk  ({:6.1} GB of {:6.1} GB)  {}{}",
                        percent(bytes as usize, entry.size_bytes.max(1) as usize),
                        bytes as f64 / 1e9,
                        entry.size_bytes as f64 / 1e9,
                        entry.path,
                        in_flight_caveat(entry.modified),
                    );
                }
            }

            // Not folded into the banner above. A file whose own stat failed is
            // unmeasured on a platform that measures, and letting its measured
            // neighbours suppress the notice would leave it looking whole -
            // the same silent clean bill of health the banner exists to
            // prevent, one file at a time instead of all of them at once.
            let unmeasured: Vec<_> = report.unmeasured().collect();
            if !unmeasured.is_empty() {
                let _ = writeln!(
                    out,
                    "\n🕳️  {} file(s) could not be stat'd, so nothing is claimed about how much of them is on disk:",
                    unmeasured.len()
                );
                for entry in unmeasured {
                    let _ = writeln!(out, "   {}", entry.path);
                }
            }
        }

        for (cost, heading) in [
            (Cost::Reencode, "🔥 Why a re-encode is needed:"),
            (Cost::Remux, "🔁 Why a remux is needed:"),
        ] {
            let tally = tally(
                report
                    .bucket(Some(cost))
                    .flat_map(|entry| entry_findings(entry, Conformance::reasons)),
                Finding::describe,
            );
            if tally.is_empty() {
                continue;
            }

            let _ = writeln!(out, "\n{heading}");
            let _ = writeln!(out, "─────────────────────────────");
            for ((claim, source), count) in tally {
                let _ = writeln!(out, "   {count:5}  {claim}{}", mark(source));
            }
        }

        // The point of the provenance on every claim: a verdict that rests on
        // something nobody has watched happen is a prediction, and the reader
        // is the one who can go and measure it.
        let unverified = tally(
            report
                .entries
                .iter()
                .flat_map(|entry| entry_findings(entry, Conformance::unverified)),
            Finding::claim,
        );
        if !unverified.is_empty() {
            let _ = writeln!(out, "\n❓ Verdicts above resting on unverified claims:");
            let _ = writeln!(out, "───────────────────────────────────────────────");
            for ((claim, _), count) in unverified {
                let _ = writeln!(out, "   {count:5}  {claim}");
            }
        }

        let needing_work: Vec<_> = report
            .bucket(Some(Cost::Reencode))
            .chain(report.bucket(Some(Cost::Remux)))
            .collect();
        if !needing_work.is_empty() {
            let _ = writeln!(out, "\n📄 Per file:");
            let _ = writeln!(out, "────────────");
            let mut listed: Vec<_> = needing_work;
            listed.sort_by(|left, right| left.path.cmp(&right.path));
            for entry in listed {
                let Outcome::Assessed(conformance) = &entry.outcome else {
                    continue;
                };
                let claims: Vec<String> = conformance
                    .reasons()
                    .iter()
                    .map(|finding| format!("{}{}", finding.describe(), mark(finding.source)))
                    .collect();
                let _ = writeln!(
                    out,
                    "   [{}] {}  {}",
                    match conformance.cost() {
                        Some(Cost::Reencode) => "ENCODE",
                        _ => "REMUX ",
                    },
                    entry.path,
                    claims.join("; ")
                );
            }
        }

        if unreadable > 0 {
            let _ = writeln!(out, "\n❓ Could not be probed:");
            let _ = writeln!(out, "───────────────────────");
            for entry in report.unreadable() {
                if let Outcome::ProbeFailed { reason } = &entry.outcome {
                    let _ = writeln!(out, "   {}\n   {reason}", entry.path);
                }
            }
        }

        let _ = writeln!(
            out,
            "\n   ? marks a claim taken from a specification and never measured on the device.\n   Nothing has been changed on disk; audit only reports."
        );

        out
    }
}

/// One of the four directories a work root is made of.
fn is_queue_directory(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            QueueDirectory::ALL
                .iter()
                .any(|directory| name == directory.on_disk_name())
        })
        .unwrap_or(false)
}

/// How long after a write a file is still plausibly being written.
///
/// A copy touches the mtime on every write, so a file written moments ago is
/// most likely still arriving and its holes are preallocation rather than loss.
/// The window is deliberately generous - a transfer that stalls briefly should
/// not lose the caveat - because being wrong costs a hedge on a line rather
/// than a missing line: the file is listed either way.
const IN_FLIGHT_WINDOW: Duration = Duration::from_secs(15 * 60);

/// The caveat in the section heading, restated on a line that may be read on
/// its own. Empty unless the file was written recently enough for it to apply.
fn in_flight_caveat(modified: Option<SystemTime>) -> &'static str {
    let recent = modified
        .and_then(|at| SystemTime::now().duration_since(at).ok())
        .is_some_and(|age| age < IN_FLIGHT_WINDOW);

    if recent {
        "  (written just now - may still be arriving)"
    } else {
        ""
    }
}

fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        100.0 * part as f64 / whole as f64
    }
}

fn mark(source: Provenance) -> &'static str {
    if source.is_assumed() {
        " ?"
    } else {
        ""
    }
}

fn entry_findings(
    entry: &AuditEntry,
    which: fn(&Conformance) -> &[Finding],
) -> impl Iterator<Item = &Finding> {
    match &entry.outcome {
        Outcome::Assessed(conformance) => which(conformance).iter(),
        Outcome::ProbeFailed { .. } => [].iter(),
    }
}

/// Count findings, keyed by whichever reading of them the section is about. A
/// per-file listing of 2400 files is unreadable; what a reader acts on is which
/// line accounts for the most of them.
fn tally<'a>(
    findings: impl Iterator<Item = &'a Finding>,
    key: fn(&Finding) -> String,
) -> Vec<((String, Provenance), usize)> {
    let mut counts: BTreeMap<(String, Provenance), usize> = BTreeMap::new();
    for finding in findings {
        *counts.entry((key(finding), finding.source)).or_default() += 1;
    }

    let mut tallied: Vec<_> = counts.into_iter().collect();
    tallied.sort_by(|left, right| right.1.cmp(&left.1).then(left.0.cmp(&right.0)));
    tallied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use tempfile::TempDir;

    fn ffmpeg_present() -> bool {
        let available = Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if !available {
            assert!(
                std::env::var("CI").is_err(),
                "FFmpeg must be installed in CI: without it the audit is never run against a real file"
            );
            eprintln!("skipping: ffmpeg is not on PATH");
        }

        available
    }

    /// A one-second file, encoded the way the argument says.
    fn build(path: &Path, args: &[&str]) {
        let built = Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=320x240:rate=24",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
            ])
            .args(args)
            .args(["-y"])
            .arg(path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build {path:?}");
    }

    #[tokio::test]
    async fn an_empty_directory_audits_to_nothing() {
        let dir = TempDir::new().unwrap();
        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();

        let report = command.execute().await.unwrap();

        assert!(report.entries.is_empty());
        assert!(command.render_report(&report).contains("No media files"));
    }

    #[tokio::test]
    async fn an_unknown_target_is_refused_before_anything_is_walked() {
        let dir = TempDir::new().unwrap();

        assert!(AuditCommand::new(dir.path().to_path_buf(), "living-room-tv").is_err());
    }

    #[tokio::test]
    async fn sorts_a_real_library_into_cost_buckets() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("Series")).unwrap();

        // Only encoders the rest of this project's tests already prove are in
        // CI's FFmpeg build: an audit test is not the place to discover which
        // build the runner shipped.
        let h264 = [
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
        ];

        build(
            &dir.path().join("conforms.mp4"),
            &[&h264[..], &["-c:a", "aac", "-ac", "2"]].concat(),
        );
        build(
            &dir.path().join("Series/surround.mkv"),
            &[&h264[..], &["-c:a", "aac", "-ac", "6"]].concat(),
        );
        build(
            &dir.path().join("Series/mpeg4.avi"),
            &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"],
        );

        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();
        let report = command.execute().await.unwrap();

        assert_eq!(report.entries.len(), 3);
        assert_eq!(report.bucket(None).count(), 1);
        assert_eq!(report.bucket(Some(Cost::Remux)).count(), 1);
        assert_eq!(report.bucket(Some(Cost::Reencode)).count(), 1);

        let rendered = command.render_report(&report);
        assert!(rendered.contains("Series/surround.mkv"), "{rendered}");
        assert!(
            rendered.contains("audio channels: 6 (accepts up to 2)"),
            "{rendered}"
        );

        // Every file has a size, and the buckets are reported in GB from it.
        assert!(report.entries.iter().all(|entry| entry.size_bytes > 0));
    }

    /// The one playback failure this project has actually watched: an AVI the
    /// LG plays and then stalls on. Auditing it against the Chromecast hides
    /// the container behind the video codec, which is how this was missed.
    #[tokio::test]
    async fn an_avi_is_not_reported_as_fine_on_the_lg() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        // No audio track: the container is then the only thing that can fail,
        // which is exactly the claim under test.
        build(&dir.path().join("stalls.avi"), &["-c:v", "mpeg4", "-an"]);

        let command = AuditCommand::new(dir.path().to_path_buf(), "lg-cx-webos").unwrap();
        let report = command.execute().await.unwrap();

        assert_eq!(
            report.bucket(None).count(),
            0,
            "an AVI is not 'already fine'"
        );
        assert_eq!(report.bucket(Some(Cost::Remux)).count(), 1);
        assert!(
            command.render_report(&report).contains("container: avi"),
            "{}",
            command.render_report(&report)
        );
    }

    /// A work root inside the media root is the mistake CLAUDE.md says people
    /// make constantly, and its contents are not library files.
    #[tokio::test]
    async fn the_queues_own_directories_are_never_audited() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        for queue_dir in ["_queue", "_in_progress", "_completed", "_failed"] {
            std::fs::create_dir_all(dir.path().join(queue_dir)).unwrap();
            build(
                &dir.path().join(queue_dir).join("half-written.mp4"),
                &["-c:v", "libx264", "-c:a", "aac"],
            );
        }

        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();

        assert!(command.execute().await.unwrap().entries.is_empty());
    }

    #[tokio::test]
    async fn a_file_ffprobe_cannot_read_is_reported_rather_than_judged() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("broken.mp4"), b"not a media file").unwrap();

        let command = AuditCommand::new(dir.path().to_path_buf(), "lg-cx-webos").unwrap();
        let report = command.execute().await.unwrap();

        assert_eq!(report.unreadable().count(), 1);
        assert_eq!(report.bucket(None).count(), 0);
        assert!(command
            .render_report(&report)
            .contains("Could not be probed"));
    }

    /// Write `head`, then extend the file to `apparent` bytes.
    ///
    /// Returns whether the extension was actually left as a hole. A filesystem
    /// that allocated it instead leaves nothing to measure, and the caller must
    /// say so and stop rather than assert something weaker.
    #[cfg(unix)]
    fn punch(path: &Path, head: &[u8], apparent: u64) -> bool {
        use std::io::Write;

        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(head).unwrap();
        file.set_len(apparent).unwrap();
        file.sync_all().unwrap();
        drop(file);

        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.len(), apparent);

        let allocation = allocation_of(&metadata);
        let holed = matches!(
            allocation,
            Allocation::Measured { allocated_bytes } if allocated_bytes < apparent
        );

        if !holed {
            // On Linux this is a failure, not a skip. `cargo test` captures a
            // passing test's stderr, so a skip and a real measurement leave
            // identical job logs, and a green CI run would then prove nothing
            // about whether a hole was ever created. Every filesystem the CI
            // job can land on - ext4, tmpfs, overlayfs - supports holes, so
            // there is nothing here to be tolerant of.
            #[cfg(target_os = "linux")]
            panic!(
                "no hole was created under {path:?} ({allocation:?}); the sparse-file \
                 tests are only meaningful against a real hole, and on Linux that is \
                 required rather than skipped"
            );

            // Elsewhere it stays a skip, and is audible: the platforms that
            // reach this line are read by a person at their own terminal.
            #[cfg(not(target_os = "linux"))]
            eprintln!(
                "skipping: the filesystem under {path:?} allocated the extension \
                 rather than leaving a hole, so there is no sparse file to measure ({allocation:?})"
            );
        }

        holed
    }

    /// The tolerances, stated as the cases they exist to separate. No disk:
    /// this is the arithmetic, and the two tests below it are the real holes.
    #[test]
    fn a_shortfall_is_reported_only_when_it_is_both_a_megabyte_and_a_twentieth() {
        let measured = |allocated_bytes| Allocation::Measured { allocated_bytes };

        // The largest sparse file on the real library, from issue #189.
        assert_eq!(
            measured(2_173_702_144).shortfall(4_525_286_741),
            Some(2_351_584_597)
        );
        // Block rounding goes the other way: allocated exceeds apparent.
        assert_eq!(measured(8192).shortfall(5000), None);
        // Wholly inlined, but under the floor - a small file charged no blocks.
        assert_eq!(measured(0).shortfall(512 * 1024), None);
        // Over the floor, under the share: 4 MiB off 4 GiB is accounting, or a
        // filesystem compressor getting a tenth of a percent off the payload.
        assert_eq!(measured((4 << 30) - (4 << 20)).shortfall(4 << 30), None);
        // And a platform that cannot measure never reports a file as whole.
        assert_eq!(Allocation::Unmeasurable.shortfall(4 << 30), None);
    }

    #[test]
    fn the_in_flight_caveat_is_attached_to_a_line_that_may_be_read_on_its_own() {
        let now = SystemTime::now();

        assert!(in_flight_caveat(Some(now)).contains("may still be arriving"));
        assert_eq!(
            in_flight_caveat(Some(now - Duration::from_secs(24 * 3600))),
            ""
        );
        assert_eq!(in_flight_caveat(None), "");
    }

    /// A file whose own stat failed must not hide behind its measured
    /// neighbours - that is the per-file version of the clean bill of health
    /// the platform banner exists to prevent.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_whose_stat_failed_is_named_rather_than_left_looking_whole() {
        fn entry(path: &str, allocation: Allocation) -> AuditEntry {
            AuditEntry {
                path: path.to_string(),
                size_bytes: 1000,
                allocation,
                modified: None,
                outcome: Outcome::ProbeFailed {
                    reason: "fixture".into(),
                },
            }
        }

        let dir = TempDir::new().unwrap();
        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();
        let report = AuditReport {
            target: "chromecast-gen2-3".into(),
            scan_path: dir.path().to_path_buf(),
            audit_time: Duration::from_secs(0),
            entries: vec![
                entry(
                    "measured.mp4",
                    Allocation::Measured {
                        allocated_bytes: 1000,
                    },
                ),
                entry("unstattable.mp4", Allocation::Unmeasurable),
            ],
        };

        let rendered = command.render_report(&report);
        assert!(
            rendered.contains("1 file(s) could not be stat'd"),
            "the measured neighbour must not suppress or absorb it: {rendered}"
        );
        assert!(rendered.contains("unstattable.mp4"), "{rendered}");
    }

    /// A real hole, and a file of exactly the same apparent size that has none.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_hole_is_measured_and_a_solid_file_of_the_same_size_is_not() {
        const SIZE: u64 = 16 << 20;

        let dir = TempDir::new().unwrap();
        let sparse = dir.path().join("sparse.bin");
        let solid = dir.path().join("solid.bin");

        if !punch(&sparse, b"header", SIZE) {
            return;
        }
        std::fs::write(&solid, vec![7u8; SIZE as usize]).unwrap();

        let missing = allocation_of(&std::fs::metadata(&sparse).unwrap())
            .shortfall(SIZE)
            .expect("a 16 MiB hole is over both tolerances");
        assert!(
            missing > SIZE - (1 << 20),
            "nearly all of it, got {missing}"
        );

        let solid_allocation = allocation_of(&std::fs::metadata(&solid).unwrap());
        assert_eq!(
            solid_allocation.shortfall(SIZE),
            None,
            "a file with no hole is not reported: {solid_allocation:?}"
        );
    }

    /// The whole of issue #189: FFprobe reads a sparse file's header and calls
    /// it conforming, which stays true - and the missing content is now said
    /// alongside that verdict rather than instead of it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_sparse_file_keeps_its_verdict_and_is_reported_as_incomplete() {
        if !ffmpeg_present() {
            return;
        }

        const HOLE: u64 = 32 << 20;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("half-there.mp4");
        build(
            &path,
            &[
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-ac",
                "2",
                "-movflags",
                "+faststart",
            ],
        );

        // The hole goes in a `free` box whose declared length covers it, so the
        // file stays structurally whole. What is under test is the measurement
        // and that the verdict above it is untouched - not how the mov demuxer
        // reacts to a zeroed payload, which would be a different bug's test.
        let mut head = std::fs::read(&path).unwrap();
        head.extend_from_slice(&(HOLE as u32).to_be_bytes());
        head.extend_from_slice(b"free");
        let apparent = head.len() as u64 + HOLE - 8;
        if !punch(&path, &head, apparent) {
            return;
        }

        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();
        let report = command.execute().await.unwrap();

        assert_eq!(
            report.bucket(None).count(),
            1,
            "the conformance verdict is not touched: it still Direct Plays"
        );

        let (entry, missing) = report.incomplete().next().expect("the hole is reported");
        assert_eq!(entry.path, "half-there.mp4");
        assert!(missing > 31 << 20, "got {missing}");

        let rendered = command.render_report(&report);
        assert!(rendered.contains("not on disk"), "{rendered}");
        assert!(rendered.contains("half-there.mp4"), "{rendered}");
    }

    /// A platform with no `st_blocks` says it did not look. Reporting 0%
    /// missing would read as a clean bill of health for an unrun check.
    #[cfg(not(unix))]
    #[tokio::test]
    async fn where_allocation_cannot_be_read_the_report_says_so_rather_than_zero() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        build(
            &dir.path().join("whole.mp4"),
            &["-c:v", "libx264", "-c:a", "aac"],
        );

        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();
        let report = command.execute().await.unwrap();

        assert!(report
            .entries
            .iter()
            .all(|entry| entry.allocation == Allocation::Unmeasurable));
        assert_eq!(report.incomplete().count(), 0);

        let rendered = command.render_report(&report);
        assert!(
            rendered.contains("Not measurable on this platform"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn ignored_paths_are_never_probed() {
        if !ffmpeg_present() {
            return;
        }

        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("extras")).unwrap();
        std::fs::write(dir.path().join(".plexifyignore"), "extras/\n").unwrap();
        build(
            &dir.path().join("extras/skipped.mp4"),
            &["-c:v", "libx264", "-c:a", "aac"],
        );

        let command = AuditCommand::new(dir.path().to_path_buf(), "chromecast-gen2-3").unwrap();

        assert!(command.execute().await.unwrap().entries.is_empty());
    }
}
