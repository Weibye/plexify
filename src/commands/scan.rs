use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};
use walkdir::WalkDir;

use crate::ignore::IgnoreFilter;
use crate::job::{MediaFileType, Operation};
use crate::probe::probe;
use crate::queue::JobQueue;
use crate::target::{evaluate, PlaybackTarget};

use super::job_processor::{operation_for, JobProcessResult, JobProcessor, JobProcessorConfig};

/// What a scan decided about one file.
enum Resolution {
    /// Queue this work.
    Work(Operation),
    /// Every target plays it as it is, so there is no job to make.
    Conforms,
    /// FFprobe could not read it, so nothing is known about what it needs.
    Unreadable(String),
}

/// Command to scan a directory for media files and create jobs
pub struct ScanCommand {
    media_root: PathBuf,
    work_root: PathBuf,
    preset: Option<String>,
    /// The clients every queued file has to play on. Empty means none was
    /// named, and the extension decides as it did before.
    targets: Vec<PlaybackTarget>,
}

impl ScanCommand {
    pub fn new(
        media_root: PathBuf,
        work_root: PathBuf,
        preset: Option<String>,
        targets: &[String],
    ) -> Result<Self> {
        Ok(Self {
            media_root,
            work_root,
            preset,
            targets: targets
                .iter()
                .map(|spec| PlaybackTarget::load(spec))
                .collect::<Result<Vec<_>>>()?,
        })
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.media_root.exists() {
            return Err(anyhow!(
                "Media directory does not exist: {:?}",
                self.media_root
            ));
        }

        if !self.media_root.is_dir() {
            return Err(anyhow!("Path is not a directory: {:?}", self.media_root));
        }

        info!("🔎 Scanning directory: {:?}", self.media_root);
        info!("📁 Recursively scanning all subdirectories...");

        // Initialize ignore filter
        let ignore_filter = match IgnoreFilter::new(self.media_root.clone()) {
            Ok(filter) => Some(filter),
            Err(e) => {
                warn!("Failed to load .plexifyignore patterns: {}", e);
                None
            }
        };

        let queue = JobQueue::new(self.media_root.clone(), self.work_root.clone());
        queue.init().await?;

        let mut media_files: Vec<(PathBuf, MediaFileType)> = Vec::new();
        let mut directories_scanned = std::collections::HashSet::new();
        let mut ignored_count = 0;
        let mut files_processed = 0;

        // Create a progress bar for scanning
        let scan_pb = ProgressBar::new_spinner();
        scan_pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        scan_pb.set_message("Scanning directories...");
        scan_pb.enable_steady_tick(std::time::Duration::from_millis(120));

        // Walk through the directory to find media files
        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_entry(|e| {
                let path = e.path();

                // Always allow the root directory
                if path == self.media_root {
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

            // Check if this individual path should be ignored
            if let Some(ref filter) = ignore_filter {
                if filter.should_ignore(path) {
                    debug!("🚫 Ignoring path: {:?}", path);
                    ignored_count += 1;
                    // Skip this entry completely
                    continue;
                }
            }

            // Track directories being scanned for better user feedback
            if path.is_dir() && path != self.media_root {
                if let Ok(relative_dir) = path.strip_prefix(&self.media_root) {
                    if !directories_scanned.contains(relative_dir) {
                        directories_scanned.insert(relative_dir.to_path_buf());
                        scan_pb.set_message(format!("Scanning: {:?}", relative_dir));
                    }
                }
            }

            if path.is_file() {
                files_processed += 1;

                // Update progress bar message periodically
                if files_processed % 100 == 0 {
                    scan_pb.set_message(format!("Processed {} files...", files_processed));
                }

                if let Ok(file_type) = JobProcessor::determine_file_type(path) {
                    if let Ok(relative_path) = path.strip_prefix(&self.media_root) {
                        media_files.push((relative_path.to_path_buf(), file_type));
                    }
                }
            }
        }

        scan_pb.finish_and_clear();

        info!(
            "📊 Scanned {} directories, processed {} files, and found {} media files",
            directories_scanned.len(),
            files_processed,
            media_files.len()
        );

        if ignored_count > 0 {
            info!(
                "📋 Ignored {} paths due to .plexifyignore patterns",
                ignored_count
            );
        }

        if !directories_scanned.is_empty() {
            debug!(
                "📋 Scanned subdirectories: {:?}",
                directories_scanned.iter().collect::<Vec<_>>()
            );
        }

        info!("🔄 Now creating transcoding jobs...");

        let mut job_count = 0;
        let total_files = media_files.len();

        let job_pb = if total_files > 0 {
            let pb = ProgressBar::new(total_files as u64);
            pb.set_style(
                ProgressStyle::with_template("Creating jobs {bar:30.cyan/blue} {pos}/{len} {msg}")
                    .unwrap()
                    .progress_chars("█▉▊▋▌▍▎▏ "),
            );
            Some(pb)
        } else {
            None
        };

        // Get configuration settings for jobs
        let config = JobProcessorConfig::from_preset(self.preset.as_deref())?;
        let processor = JobProcessor::new(&queue, &config, &self.media_root);

        let resolutions = self.resolve(&media_files).await?;
        let mut conforming = 0;
        let mut unreadable = Vec::new();

        for ((path, file_type), resolution) in media_files.iter().zip(resolutions) {
            if let Some(ref pb) = job_pb {
                pb.set_message(format!(
                    "{file_type:?}: {:?}",
                    path.file_name().unwrap_or_default()
                ));
                pb.inc(1);
            }

            let operation = match resolution {
                Resolution::Work(operation) => operation,
                Resolution::Conforms => {
                    conforming += 1;
                    debug!("Every target plays {path:?} as it is");
                    continue;
                }
                // Doing nothing is the only answer here that cannot be wrong.
                // Queueing a re-encode would spend days on a file that may need
                // nothing, and passing over it quietly would hide a file that
                // may need everything - so it is named instead.
                Resolution::Unreadable(reason) => {
                    unreadable.push((path.clone(), reason));
                    continue;
                }
            };

            let result = processor
                .process_media_file(path, file_type.clone(), operation)
                .await?;
            processor.log_result(path, file_type, &result);

            if result == JobProcessResult::Created {
                job_count += 1;
            }
        }

        if let Some(pb) = job_pb {
            pb.finish_and_clear();
        }

        if conforming > 0 {
            info!("✅ {conforming} files already play on every target, and were not queued.");
        }

        for (path, reason) in &unreadable {
            warn!("⚠️ Not queued, because FFprobe could not read it: {path:?}: {reason}");
        }
        if !unreadable.is_empty() {
            warn!(
                "⚠️ {} files could not be probed. Nothing is known about what they need, so nothing was decided for them.",
                unreadable.len()
            );
        }

        info!(
            "✅ Scan complete. Added {} new jobs to the queue.",
            job_count
        );
        Ok(())
    }

    /// What each file needs, asked of every target.
    ///
    /// With no target named nothing is probed and the extension decides, which
    /// is what this command did before it could ask. Naming a target costs one
    /// FFprobe per file, and no more than one however many targets are named.
    async fn resolve(&self, files: &[(PathBuf, MediaFileType)]) -> Result<Vec<Resolution>> {
        if self.targets.is_empty() {
            return Ok(files
                .iter()
                .map(|(_, file_type)| Resolution::Work(operation_for(file_type)))
                .collect());
        }

        info!(
            "🔍 Probing {} files against {}...",
            files.len(),
            self.targets
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );

        let paths: Vec<PathBuf> = files
            .iter()
            .map(|(path, _)| self.media_root.join(path))
            .collect();
        let targets = self.targets.clone();

        let probe_pb = ProgressBar::new(paths.len() as u64);
        probe_pb.set_style(
            ProgressStyle::with_template("Probing {bar:30.cyan/blue} {pos}/{len}")
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏ "),
        );

        // A wall of blocking subprocesses, so they run on the blocking pool
        // rather than parking a runtime worker for the length of a library.
        let resolutions = tokio::task::spawn_blocking(move || {
            let resolutions = paths
                .par_iter()
                .map(|path| {
                    let resolution = resolve_one(path, &targets);
                    probe_pb.inc(1);
                    resolution
                })
                .collect();
            probe_pb.finish_and_clear();
            resolutions
        })
        .await?;

        Ok(resolutions)
    }
}

/// What one file needs in order to play on every one of `targets`.
fn resolve_one(path: &Path, targets: &[PlaybackTarget]) -> Resolution {
    let media = match probe(path) {
        Ok(media) => media,
        Err(error) => return Resolution::Unreadable(format!("{error:#}")),
    };

    // One probe, every target: the file is read once however many clients are
    // being satisfied, and a conforming answer from all of them is no job.
    let operation = targets
        .iter()
        .filter_map(|target| Operation::for_conformance(&evaluate(&media, target), target))
        .reduce(Operation::hardest);

    match operation {
        Some(operation) => Resolution::Work(operation),
        None => Resolution::Conforms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{AudioAction, SubtitleAction};
    use crate::target::Conformance;
    use std::fs;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
            &[],
        )
        .unwrap();

        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_nonexistent_directory() {
        let scan_cmd = ScanCommand::new(
            PathBuf::from("/nonexistent/path"),
            PathBuf::from("/tmp"),
            None,
            &[],
        )
        .unwrap();

        let result = scan_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_scan_with_preset() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
            &[],
        )
        .unwrap();

        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_with_invalid_preset() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("invalid_preset".to_string()),
            &[],
        )
        .unwrap();

        let result = scan_cmd.execute().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_scan_hierarchical_directories() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create hierarchical directory structure
        fs::create_dir_all(media_root.join("show1/season1")).unwrap();
        fs::create_dir_all(media_root.join("show2/season2")).unwrap();
        fs::create_dir_all(media_root.join("movies")).unwrap();
        fs::create_dir_all(media_root.join("very/deep/nested/folder")).unwrap();

        // Create media files in different subdirectories
        fs::write(media_root.join("show1/season1/episode1.webm"), "").unwrap();
        fs::write(media_root.join("show1/season1/episode1.vtt"), "").unwrap();
        fs::write(media_root.join("show2/season2/episode2.mkv"), "").unwrap();
        fs::write(media_root.join("movies/movie1.mkv"), "").unwrap();
        fs::write(media_root.join("very/deep/nested/folder/deep.webm"), "").unwrap();
        fs::write(media_root.join("very/deep/nested/folder/deep.vtt"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Verify queue directory was created and contains job files
        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Count job files - should have created jobs for all media files with proper subtitles
        let job_files: Vec<_> = fs::read_dir(&queue_dir)
            .unwrap()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension()? == "job" {
                    Some(path)
                } else {
                    None
                }
            })
            .collect();

        // Should have 4 jobs: 2 webm files with subtitles + 2 mkv files
        assert_eq!(job_files.len(), 4);
    }

    #[tokio::test]
    async fn test_scan_finds_nested_media_files() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create a deep nested structure
        let deep_path = media_root.join("level1/level2/level3/level4");
        fs::create_dir_all(&deep_path).unwrap();

        // Create media file at different depths
        fs::write(media_root.join("root.mkv"), "").unwrap();
        fs::write(media_root.join("level1/l1.mkv"), "").unwrap();
        fs::write(deep_path.join("deep.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        // Should find all 3 mkv files regardless of nesting depth
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        assert_eq!(job_count, 3);
    }

    #[tokio::test]
    async fn test_scan_mixed_media_types_in_hierarchy() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create different media types in different folders
        fs::create_dir_all(media_root.join("webm_folder")).unwrap();
        fs::create_dir_all(media_root.join("mkv_folder")).unwrap();
        fs::create_dir_all(media_root.join("mixed_folder")).unwrap();

        // WebM files (need matching VTT)
        fs::write(media_root.join("webm_folder/video1.webm"), "").unwrap();
        fs::write(media_root.join("webm_folder/video1.vtt"), "").unwrap();
        fs::write(media_root.join("webm_folder/video2.webm"), "").unwrap(); // No VTT - should be skipped

        // MKV files
        fs::write(media_root.join("mkv_folder/video1.mkv"), "").unwrap();
        fs::write(media_root.join("mkv_folder/video2.mkv"), "").unwrap();

        // Mixed folder
        fs::write(media_root.join("mixed_folder/mixed1.webm"), "").unwrap();
        fs::write(media_root.join("mixed_folder/mixed1.vtt"), "").unwrap();
        fs::write(media_root.join("mixed_folder/mixed2.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            Some("quality".to_string()),
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should create jobs for: 2 webm files with VTT + 3 mkv files = 5 jobs
        // (video2.webm without VTT should be skipped)
        assert_eq!(job_count, 5);
    }

    fn ffmpeg_present() -> bool {
        let available = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);

        if !available {
            assert!(
                std::env::var("CI").is_err(),
                "FFmpeg must be installed in CI: these tests are the only check that a scan asks a real file what it needs"
            );
            eprintln!("skipping: ffmpeg is not on PATH");
        }

        available
    }

    fn build(path: &Path, args: &[&str]) {
        let built = std::process::Command::new("ffmpeg")
            .args([
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=duration=1",
            ])
            .args(args)
            .arg("-y")
            .arg(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(built.success(), "could not build {path:?}");
    }

    /// Every job the queue holds, ordered by the file it transcodes.
    fn queued_jobs(work_root: &Path) -> Vec<crate::job::Job> {
        let mut jobs: Vec<crate::job::Job> = fs::read_dir(work_root.join("_queue"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                if path.extension()? != "job" {
                    return None;
                }
                serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()
            })
            .collect();
        jobs.sort_by(|left, right| left.input_path.cmp(&right.input_path));
        jobs
    }

    /// Every job the queue holds, by the file it transcodes.
    fn queued(work_root: &Path) -> Vec<(String, Operation)> {
        queued_jobs(work_root)
            .into_iter()
            .map(|job| {
                (
                    job.input_path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                    job.operation,
                )
            })
            .collect()
    }

    /// The whole point of asking a client before queueing: a file it already
    /// plays produces no job at all.
    #[tokio::test]
    async fn a_file_the_target_already_plays_is_not_queued() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // H.264 high, 8-bit, stereo AAC: what the Chromecast is observed to
        // Direct Play, in an MKV that only needs its container changed.
        build(
            &media_root.join("conforms.mp4"),
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
            ],
        );
        build(
            &media_root.join("surround.mkv"),
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
                "6",
            ],
        );

        ScanCommand::new(
            media_root.to_path_buf(),
            media_root.to_path_buf(),
            None,
            &["chromecast-gen2-3".to_string()],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();

        // The MP4 is not a job because nothing needs doing to it - not because
        // scan does not look at MP4s, which it never has.
        assert_eq!(
            queued(media_root),
            vec![(
                "surround.mkv".to_string(),
                Operation::Remux {
                    audio: AudioAction::Transcode { channels: Some(2) },
                    subtitles: SubtitleAction::Keep,
                }
            )],
            "only the file that needs work is queued, and only for the track that is wrong"
        );
    }

    /// The rule that decides a quarter of this library. The LG decodes MPEG-4
    /// and the Chromecast does not, so the same AVI is a remux against one and
    /// a re-encode against the pair.
    #[tokio::test]
    async fn naming_two_targets_queues_the_work_that_satisfies_both() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();
        build(
            &media_root.join("film.avi"),
            &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"],
        );

        let scan = |targets: Vec<String>, work: PathBuf| {
            let media_root = media_root.to_path_buf();
            async move {
                fs::create_dir_all(&work).unwrap();
                ScanCommand::new(media_root, work.clone(), None, &targets)
                    .unwrap()
                    .execute()
                    .await
                    .unwrap();
                queued(&work)
            }
        };

        let lg = scan(vec!["lg-cx-webos".to_string()], media_root.join("lg")).await;
        assert_eq!(
            lg,
            vec![(
                "film.avi".to_string(),
                Operation::Remux {
                    audio: AudioAction::Copy,
                    subtitles: SubtitleAction::Keep,
                }
            )],
            "the LG decodes MPEG-4, so only the container is wrong"
        );

        let both = scan(
            vec!["lg-cx-webos".to_string(), "chromecast-gen2-3".to_string()],
            media_root.join("both"),
        )
        .await;
        assert_eq!(
            both,
            vec![(
                "film.avi".to_string(),
                Operation::Reencode { channels: None }
            )],
            "the Chromecast does not, and a file that plays on only one device \
             is one the Pi is asked to transcode when it is played on the other"
        );
    }

    /// A file one target already Direct Plays is re-encoded to the other
    /// target's envelope, and the copy the first was playing is not kept.
    ///
    /// This is a description of a gap, not of an intended behaviour, and it
    /// asserts what the code does today so that changing it is deliberate.
    /// `resolve_one` folds every target's answer into one `Operation` and
    /// `Job::new` derives one output from the input's name, so a pair of
    /// clients that disagree has exactly one destination to land in - and
    /// `hardest` decides it belongs to the client that needs more work. 869
    /// files in this library sit in exactly this position: the LG plays them
    /// as they are, the Chromecast does not.
    ///
    /// The premise is measured here rather than assumed, because the test
    /// says nothing about the fold unless the two verdicts really differ.
    #[tokio::test]
    async fn a_file_one_target_already_plays_is_re_encoded_for_the_other_and_not_kept() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();
        let source = media_root.join("legacy.mkv");

        // MPEG-4 video in Matroska, stereo AAC. Nothing but the video codec
        // separates the two targets on it.
        build(&source, &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"]);

        let probed = crate::probe::probe(&source).unwrap();
        assert!(
            matches!(
                evaluate(&probed, &PlaybackTarget::load("lg-cx-webos").unwrap()),
                Conformance::Conforms { .. }
            ),
            "the fixture has to Direct Play on the LG for this test to be about anything"
        );
        assert!(
            matches!(
                evaluate(&probed, &PlaybackTarget::load("chromecast-gen2-3").unwrap()),
                Conformance::Reencode { .. }
            ),
            "and it has to need its picture re-encoded for the Chromecast"
        );

        // Scanning against the LG alone queues nothing, which is what the
        // pair below takes away.
        let lg_only = media_root.join("lg");
        fs::create_dir_all(&lg_only).unwrap();
        ScanCommand::new(
            media_root.to_path_buf(),
            lg_only.clone(),
            None,
            &["lg-cx-webos".to_string()],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();
        assert!(queued_jobs(&lg_only).is_empty());

        let both = media_root.join("both");
        fs::create_dir_all(&both).unwrap();
        ScanCommand::new(
            media_root.to_path_buf(),
            both.clone(),
            None,
            &["lg-cx-webos".to_string(), "chromecast-gen2-3".to_string()],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();

        let jobs = queued_jobs(&both);
        assert_eq!(jobs.len(), 1, "one file, one job - there is no second one");
        assert_eq!(
            jobs[0].operation,
            Operation::Reencode { channels: None },
            "the harder target's answer is the only one queued"
        );
        assert_eq!(
            jobs[0].output_path,
            source.with_extension("mp4"),
            "and it has one destination, derived from the input's own name"
        );
    }

    #[tokio::test]
    async fn scan_queues_avi_files_alongside_the_rest() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        fs::write(media_root.join("film.avi"), "").unwrap();
        fs::write(media_root.join("episode.mkv"), "").unwrap();
        // Already an MP4, so there is nothing to do to it.
        fs::write(media_root.join("done.mp4"), "").unwrap();

        ScanCommand::new(
            media_root.to_path_buf(),
            media_root.to_path_buf(),
            None,
            &[],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();

        let job_count = fs::read_dir(media_root.join("_queue"))
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        assert_eq!(job_count, 2);
    }

    #[tokio::test]
    async fn test_scan_with_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create .plexifyignore file
        fs::write(
            media_root.join(".plexifyignore"),
            "Downloads/\n*.tmp\ntools",
        )
        .unwrap();

        // Create directory structure
        fs::create_dir_all(media_root.join("Downloads")).unwrap();
        fs::create_dir_all(media_root.join("tools")).unwrap();
        fs::create_dir_all(media_root.join("Anime")).unwrap();

        // Create media files - some should be ignored
        fs::write(media_root.join("Downloads/video1.mkv"), "").unwrap();
        fs::write(media_root.join("tools/video2.mkv"), "").unwrap();
        fs::write(media_root.join("temp.tmp"), "").unwrap();
        fs::write(media_root.join("Anime/episode1.mkv"), "").unwrap();
        fs::write(media_root.join("movie.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Check job files - should only create jobs for non-ignored files
        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should only create jobs for Anime/episode1.mkv and movie.mkv (2 jobs)
        assert_eq!(job_count, 2);
    }

    #[tokio::test]
    async fn test_scan_with_nested_plexifyignore() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create root .plexifyignore
        fs::write(media_root.join(".plexifyignore"), "*.tmp").unwrap();

        // Create nested directory with its own .plexifyignore
        fs::create_dir_all(media_root.join("Series")).unwrap();
        fs::write(
            media_root.join("Series/.plexifyignore"),
            "old/\n!important.mkv",
        )
        .unwrap();

        // Create test files
        fs::create_dir_all(media_root.join("Series/old")).unwrap();
        fs::write(media_root.join("test.tmp"), "").unwrap();
        fs::write(media_root.join("Series/show.mkv"), "").unwrap();
        fs::write(media_root.join("Series/old/episode.mkv"), "").unwrap();
        fs::write(media_root.join("Series/important.mkv"), "").unwrap();
        fs::write(media_root.join("movie.mkv"), "").unwrap();

        let scan_cmd = ScanCommand::new(
            media_root.to_path_buf(),
            temp_dir.path().to_path_buf(),
            None,
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        // Check job files
        let queue_dir = temp_dir.path().join("_queue");
        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        // Should create jobs for: Series/show.mkv, Series/important.mkv, movie.mkv (3 jobs)
        // Should ignore: test.tmp (root pattern), Series/old/episode.mkv (nested pattern)
        assert_eq!(job_count, 3);
    }
}
