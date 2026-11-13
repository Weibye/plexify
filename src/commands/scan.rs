use anyhow::{anyhow, Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::PathBuf;
use tracing::{debug, info, warn};

use crate::ignore::IgnoreFilter;
use crate::job::{MediaFileType, Operation};
use crate::naming::scope_for;
use crate::probe::{probe_all, MediaProbe};
use crate::queue::JobQueue;
use crate::target::{evaluate, Conformance, PlaybackTarget};

use super::job_processor::{operation_for, JobProcessResult, JobProcessor, JobProcessorConfig};

/// What a scan decided about one file.
enum Resolution {
    /// Queue this work.
    Work(Operation),
    /// Every target plays it as it is, so there is no job to make.
    Conforms,
    /// Audio only, or only cover art. Nothing was judged and no job can give
    /// it a picture (D33), so it is named rather than queued or passed over.
    NoPicture,
    /// FFprobe could not read it, so nothing is known about what it needs.
    Unreadable(String),
}

/// Command to scan a directory for media files and create jobs
pub struct ScanCommand {
    media_root: PathBuf,
    work_root: PathBuf,
    config: JobProcessorConfig,
    /// The clients every queued file has to play on. Empty means none was
    /// named, and the extension decides.
    targets: Vec<PlaybackTarget>,
}

impl ScanCommand {
    pub fn new(
        media_root: PathBuf,
        work_root: PathBuf,
        config: JobProcessorConfig,
        targets: &[String],
    ) -> Result<Self> {
        Ok(Self {
            media_root,
            work_root,
            config,
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

        // A rule at the library root governs every scan inside it, so the root is
        // found from an absolute path and the walk uses that same spelling for the
        // filter to match. Jobs still record paths against `media_root` as given.
        let scan_root = std::path::absolute(&self.media_root)
            .with_context(|| format!("could not resolve {}", self.media_root.display()))?;
        let scope = scope_for(&scan_root);
        let ignore_filter = IgnoreFilter::for_scope(scope.library_root, &scope.scan_path)?;

        let queue = JobQueue::new(self.media_root.clone(), self.work_root.clone());
        queue.init().await?;

        let mut media_files: Vec<(PathBuf, MediaFileType)> = Vec::new();

        // Create a progress bar for scanning
        let scan_pb = ProgressBar::new_spinner();
        scan_pb.set_style(
            ProgressStyle::with_template("{spinner:.green} {msg}")
                .unwrap()
                .tick_chars("⠁⠂⠄⡀⢀⠠⠐⠈ "),
        );
        scan_pb.set_message("Scanning directories...");
        scan_pb.enable_steady_tick(std::time::Duration::from_millis(120));

        let mut walk = ignore_filter.media_files(&scan_root);
        while let Some(path) = walk.next() {
            scan_pb.set_message(format!("Processed {} files...", walk.files));

            // The library holds more kinds of media than a scan converts.
            if let Ok(file_type) = JobProcessor::determine_file_type(&path) {
                if let Ok(relative_path) = path.strip_prefix(&scan_root) {
                    media_files.push((relative_path.to_path_buf(), file_type));
                }
            }
        }

        scan_pb.finish_and_clear();

        info!(
            "📊 Scanned {} directories, processed {} files, and found {} media files",
            walk.directories,
            walk.files,
            media_files.len()
        );

        if walk.ignored > 0 {
            info!(
                "📋 Ignored {} paths due to .plexifyignore patterns",
                walk.ignored
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

        let processor = JobProcessor::new(&queue, &self.config, &self.media_root);

        let resolutions = self.resolve(&media_files).await?;
        let mut conforming = 0;
        let mut no_picture = Vec::new();
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
                Resolution::NoPicture => {
                    no_picture.push(path.clone());
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
            processor.log_result(path, operation, &result);

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

        for path in &no_picture {
            warn!("⚠️ Not queued, because it has no picture - audio only, or only cover art: {path:?}");
        }
        if !no_picture.is_empty() {
            warn!(
                "⚠️ {} files have no picture. Nothing was judged about them, so they were not queued.",
                no_picture.len()
            );
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
    /// With no target named nothing is probed and the extension decides. Naming
    /// a target costs one FFprobe per file, and no more than one however many
    /// targets are named.
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

        probe_all(paths, move |_, media| resolve_one(media, &targets)).await
    }
}

/// What one file needs in order to play on every one of `targets`.
fn resolve_one(media: Result<MediaProbe>, targets: &[PlaybackTarget]) -> Resolution {
    let media = match media {
        Ok(media) => media,
        Err(error) => return Resolution::Unreadable(format!("{error:#}")),
    };

    // One probe, every target: the file is read once however many clients are
    // being satisfied, and a conforming answer from all of them is no job.
    let verdicts: Vec<_> = targets
        .iter()
        .map(|target| (evaluate(&media, target), target))
        .collect();
    if verdicts
        .iter()
        .any(|(conformance, _)| *conformance == Conformance::NoPicture)
    {
        return Resolution::NoPicture;
    }

    let operation = verdicts
        .iter()
        .filter_map(|(conformance, target)| Operation::for_conformance(conformance, target))
        .reduce(Operation::hardest);

    match operation {
        Some(operation) => Resolution::Work(operation),
        None => Resolution::Conforms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{AudioAction, QualityPreset, SubtitleAction};
    use crate::test_support::{build_test_file, ffmpeg_present, logs_of, queued_jobs};
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// What `--preset quality` gives when no `FFMPEG_*` override is set.
    fn quality() -> JobProcessorConfig {
        JobProcessorConfig {
            quality_settings: QualityPreset::Quality.to_quality_settings(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
            JobProcessorConfig::default(),
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
            JobProcessorConfig::default(),
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
            quality(),
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

        assert_eq!(
            job_files.len(),
            4,
            "episode1.webm and deep.webm with their .vtt, episode2.mkv and movie1.mkv"
        );
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
            quality(),
            &[],
        )
        .unwrap();
        let result = scan_cmd.execute().await;

        assert!(result.is_ok());

        let queue_dir = temp_dir.path().join("_queue");
        assert!(queue_dir.exists());

        let job_count = fs::read_dir(&queue_dir)
            .unwrap()
            .filter(|entry| entry.as_ref().unwrap().path().extension() == Some("job".as_ref()))
            .count();

        assert_eq!(job_count, 3, "root.mkv, l1.mkv and deep.mkv at any depth");
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
            quality(),
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

        assert_eq!(
            job_count, 5,
            "video1.webm and mixed1.webm with their .vtt and the three .mkv,              but not webm_folder/video2.webm, which has none"
        );
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

        // H.264 high, 8-bit, stereo AAC in Matroska: what the Chromecast is
        // observed to Direct Play. An MKV rather than an MP4, because scan
        // never reads an MP4, so one would not be queued whatever it held.
        build_test_file(
            &media_root.join("conforms.mkv"),
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
        build_test_file(
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
            JobProcessorConfig::default(),
            &["chromecast-gen2-3".to_string()],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();

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

    /// A file with no picture gets no job (D33), and it is not counted among
    /// the files that already play either: nothing measured that it does.
    #[tokio::test]
    async fn a_file_with_no_picture_is_neither_queued_nor_said_to_play() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();
        let audio_only = media_root.join("audio-only.mkv");
        build_test_file(&audio_only, &["-map", "1:a", "-c:a", "aac"]);
        // MKV carries cover art as an attached JPEG, which FFprobe lists as
        // a video stream marked `attached_pic`.
        let cover = media_root.join("cover.jpg");
        build_test_file(&cover, &["-map", "0:v", "-frames:v", "1"]);
        let cover_art = media_root.join("cover-art.mkv");
        build_test_file(
            &cover_art,
            &[
                "-map",
                "1:a",
                "-c:a",
                "aac",
                "-attach",
                cover.to_str().unwrap(),
                "-metadata:s:t",
                "mimetype=image/jpeg",
            ],
        );

        let targets = [PlaybackTarget::load("chromecast-gen2-3").unwrap()];
        for path in [&audio_only, &cover_art] {
            assert!(
                matches!(
                    resolve_one(crate::probe::probe(path), &targets),
                    Resolution::NoPicture
                ),
                "{path:?} is neither work nor a file that already plays"
            );
        }
    }

    /// The log says what a job does, which a target decides rather than the
    /// extension: against the Chromecast an MPEG-4 AVI is a re-encode, and an
    /// MKV whose only fault is 5.1 audio is a remux.
    #[test]
    fn a_queued_job_is_logged_by_what_it_does() {
        if !ffmpeg_present() {
            return;
        }

        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();
        build_test_file(
            &media_root.join("film.avi"),
            &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"],
        );
        build_test_file(
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

        let scan = ScanCommand::new(
            media_root.to_path_buf(),
            media_root.to_path_buf(),
            JobProcessorConfig::default(),
            &["chromecast-gen2-3".to_string()],
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_, logs) = logs_of(|| runtime.block_on(scan.execute()).unwrap());

        assert!(
            logs.contains(r#"Queueing re-encode job for: "film.avi""#),
            "film.avi is a re-encode against the Chromecast: {logs}"
        );
        assert!(
            logs.contains(r#"Queueing remux job for: "surround.mkv""#),
            "surround.mkv is a remux against the Chromecast: {logs}"
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
        build_test_file(
            &media_root.join("film.avi"),
            &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"],
        );

        let scan = |targets: Vec<String>, work: PathBuf| {
            let media_root = media_root.to_path_buf();
            async move {
                fs::create_dir_all(&work).unwrap();
                ScanCommand::new(
                    media_root,
                    work.clone(),
                    JobProcessorConfig::default(),
                    &targets,
                )
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
    /// asserts what the code does so that changing it is deliberate.
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
        build_test_file(&source, &["-c:v", "mpeg4", "-c:a", "aac", "-ac", "2"]);

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
            JobProcessorConfig::default(),
            &["lg-cx-webos".to_string()],
        )
        .unwrap()
        .execute()
        .await
        .unwrap();
        assert!(
            queued_jobs(&lg_only).is_empty(),
            "legacy.mkv already plays on the LG"
        );

        let both = media_root.join("both");
        fs::create_dir_all(&both).unwrap();
        ScanCommand::new(
            media_root.to_path_buf(),
            both.clone(),
            JobProcessorConfig::default(),
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
            JobProcessorConfig::default(),
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

        assert_eq!(job_count, 2, "film.avi and episode.mkv, but not done.mp4");
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
            JobProcessorConfig::default(),
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

        assert_eq!(
            job_count, 2,
            "Anime/episode1.mkv and movie.mkv, but nothing under Downloads/ or tools/"
        );
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
            JobProcessorConfig::default(),
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

        assert_eq!(
            job_count, 3,
            "Series/show.mkv, Series/important.mkv and movie.mkv,              but not Series/old/episode.mkv, which the nested rule ignores"
        );
    }

    #[tokio::test]
    async fn a_scan_of_one_show_obeys_the_library_roots_rules() {
        let temp_dir = TempDir::new().unwrap();
        let library = temp_dir.path().join("library");
        let show = library.join("Series/Show");
        fs::create_dir_all(show.join("Extras")).unwrap();
        fs::write(library.join(".plexifyignore"), "Extras/\n").unwrap();
        fs::write(show.join("Extras/bonus.mkv"), "").unwrap();

        let work = temp_dir.path().join("work");
        fs::create_dir_all(&work).unwrap();
        ScanCommand::new(show, work.clone(), JobProcessorConfig::default(), &[])
            .unwrap()
            .execute()
            .await
            .unwrap();

        assert!(
            queued_jobs(&work).is_empty(),
            "Extras/bonus.mkv is ignored by the library root's rule"
        );
    }

    #[tokio::test]
    async fn a_scan_refuses_to_run_without_its_ignore_rules() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();
        fs::create_dir_all(media_root.join("Extras")).unwrap();
        fs::write(media_root.join("Extras/bonus.mkv"), "").unwrap();
        // A Latin-1 'é' makes the whole file unreadable as UTF-8.
        fs::write(media_root.join(".plexifyignore"), b"Extras/\n# caf\xe9\n").unwrap();

        let result = ScanCommand::new(
            media_root.to_path_buf(),
            media_root.to_path_buf(),
            JobProcessorConfig::default(),
            &[],
        )
        .unwrap()
        .execute()
        .await;

        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains(".plexifyignore"), "{message}");
        assert!(!media_root.join("_queue").exists(), "nothing was queued");
    }
}
