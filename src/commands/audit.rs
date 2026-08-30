//! Report what each file in a library needs before a client will Direct Play
//! it. Read-only: it proposes nothing and changes nothing.

use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::commands::validate::MEDIA_EXTENSIONS;
use crate::ignore::IgnoreFilter;
use crate::paths::to_forward_slashes;
use crate::probe::probe;
use crate::target::{evaluate, Conformance, Cost, Finding, PlaybackTarget, Provenance};

/// One file's verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The file relative to the scanned directory, with `/` separators.
    pub path: String,
    pub size_bytes: u64,
    pub outcome: Outcome,
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

        AuditEntry {
            path: to_forward_slashes(relative),
            size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
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
