use anyhow::Result;
use std::path::Path;
use tracing::{debug, info, warn};

use crate::job::{
    AudioAction, Job, MediaFileType, Operation, PostProcessingSettings, QualitySettings,
    SubtitleAction,
};
use crate::queue::JobQueue;

/// Shared job processing configuration
pub struct JobProcessorConfig {
    pub quality_settings: QualitySettings,
    pub post_processing: PostProcessingSettings,
}

impl JobProcessorConfig {
    /// Create job processor config from preset or environment
    pub fn from_preset(preset: Option<&str>) -> Result<Self> {
        let quality_settings = match preset {
            Some(preset_name) => {
                info!("Using quality preset: '{}'", preset_name);
                QualitySettings::from_preset_name(preset_name)?
            }
            None => {
                info!("Using quality settings from environment variables");
                QualitySettings::from_env()
            }
        };
        let post_processing = PostProcessingSettings::default();

        Ok(Self {
            quality_settings,
            post_processing,
        })
    }
}

/// Result of attempting to process a job
#[derive(PartialEq)]
pub enum JobProcessResult {
    /// Job was successfully created and enqueued
    Created,
    /// Job was skipped because output already exists
    OutputExists,
    /// Job was skipped because it already exists in queue
    AlreadyQueued,
    /// Job was skipped because required subtitle file is missing
    MissingSubtitle,
}

/// What to do to a file, decided from its extension.
///
/// A placeholder for the conformance check (#158), which decides it from what
/// the file and the target actually hold. Everything here is measured, but the
/// extension is not what makes it true:
///
/// - All 598 `.avi` files carry MPEG-4 or H.264 video both targets decode, and
///   MP3 or AC-3 audio. Only the container is wrong, so the video is copied.
/// - No `.avi` in this library carries a subtitle stream, and 586 of the 598
///   need a full re-encode for the Chromecast whatever happens to the audio.
///   So copying the audio and keeping a track list that is always empty costs
///   nothing measurable today - and both answers become the target's to give
///   the moment #158 lands.
fn operation_for(file_type: &MediaFileType) -> Operation {
    match file_type {
        MediaFileType::Avi => Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Keep,
        },
        MediaFileType::WebM | MediaFileType::Mkv => Operation::Reencode,
    }
}

/// Shared job processor that handles the common logic between add and scan commands
pub struct JobProcessor<'a> {
    pub queue: &'a JobQueue,
    pub config: &'a JobProcessorConfig,
    pub media_root: &'a Path,
}

impl<'a> JobProcessor<'a> {
    pub fn new(queue: &'a JobQueue, config: &'a JobProcessorConfig, media_root: &'a Path) -> Self {
        Self {
            queue,
            config,
            media_root,
        }
    }

    /// Process a single media file and create a job if needed
    pub async fn process_media_file(
        &self,
        relative_path: &Path,
        file_type: MediaFileType,
    ) -> Result<JobProcessResult> {
        // Create the job
        let job = Job::new(
            relative_path.to_path_buf(),
            file_type.clone(),
            operation_for(&file_type),
            self.config.quality_settings.clone(),
            self.config.post_processing.clone(),
            self.media_root,
        );

        // Check if output already exists
        if job.output_exists(Some(self.media_root)) {
            debug!("Output already exists for: {:?}", relative_path);
            return Ok(JobProcessResult::OutputExists);
        }

        // Check if job already exists in queue
        if self.queue.job_exists(&job).await? {
            debug!("Job already exists for: {:?}", relative_path);
            return Ok(JobProcessResult::AlreadyQueued);
        }

        // For WebM files, check if required subtitle file exists
        if file_type == MediaFileType::WebM && !job.has_required_subtitle(Some(self.media_root))? {
            return Ok(JobProcessResult::MissingSubtitle);
        }

        // Create the job
        self.queue.enqueue_job(&job).await?;

        Ok(JobProcessResult::Created)
    }

    /// Log the result of job processing with appropriate messages
    pub fn log_result(
        &self,
        relative_path: &Path,
        file_type: &MediaFileType,
        result: &JobProcessResult,
    ) {
        match result {
            JobProcessResult::Created => match file_type {
                MediaFileType::WebM => {
                    info!("➕ Queueing job for: {:?}", relative_path);
                }
                MediaFileType::Mkv => {
                    info!(
                        "➕ Queueing job for: {:?} (embedded subs assumed)",
                        relative_path
                    );
                }
                MediaFileType::Avi => {
                    info!("➕ Queueing remux job for: {:?}", relative_path);
                }
            },
            JobProcessResult::OutputExists => {
                // Only debug log for scan command, add command handles this differently
            }
            JobProcessResult::AlreadyQueued => {
                // Only debug log for scan command, add command handles this differently
            }
            JobProcessResult::MissingSubtitle => {
                warn!(
                    "⚠️ SKIPPING: Missing subtitle file for '{:?}'",
                    relative_path
                );
            }
        }
    }

    /// Determine file type from extension
    pub fn determine_file_type(file_path: &Path) -> Result<MediaFileType, String> {
        match file_path.extension() {
            Some(ext) => match ext.to_string_lossy().to_lowercase().as_str() {
                "webm" => Ok(MediaFileType::WebM),
                "mkv" => Ok(MediaFileType::Mkv),
                "avi" => Ok(MediaFileType::Avi),
                _ => Err(format!(
                    "Unsupported file type. Only .webm, .mkv and .avi files are supported. Got: {:?}",
                    file_path
                )),
            },
            None => Err(format!(
                "File has no extension. Only .webm, .mkv and .avi files are supported: {:?}",
                file_path
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    /// Clear the encoding environment variables so a test observes documented
    /// defaults rather than whatever the developer's shell happens to export.
    /// Only safe inside `#[serial(env)]` tests.
    fn clear_encoding_env() {
        std::env::remove_var("FFMPEG_PRESET");
        std::env::remove_var("FFMPEG_CRF");
        std::env::remove_var("FFMPEG_AUDIO_BITRATE");
    }

    #[tokio::test]
    #[serial(env)]
    async fn test_job_processor_config_from_preset() {
        clear_encoding_env();

        let config = JobProcessorConfig::from_preset(Some("quality")).unwrap();
        // Just verify it doesn't panic and creates a config
        assert_eq!(config.quality_settings.ffmpeg_preset, "slow");
    }

    #[tokio::test]
    #[serial(env)]
    async fn test_job_processor_config_from_env() {
        clear_encoding_env();

        let config = JobProcessorConfig::from_preset(None).unwrap();
        // Just verify it doesn't panic and creates a config with defaults
        assert_eq!(config.quality_settings.ffmpeg_preset, "veryfast");
    }

    #[test]
    fn an_avi_is_queued_as_a_remux_and_everything_else_as_a_re_encode() {
        assert_eq!(
            operation_for(&MediaFileType::Avi),
            Operation::Remux {
                audio: AudioAction::Copy,
                subtitles: SubtitleAction::Keep,
            }
        );
        assert_eq!(operation_for(&MediaFileType::Mkv), Operation::Reencode);
        assert_eq!(operation_for(&MediaFileType::WebM), Operation::Reencode);
    }

    #[test]
    fn test_determine_file_type() {
        let webm_path = std::path::Path::new("video.webm");
        let mkv_path = std::path::Path::new("video.mkv");
        let mp4_path = std::path::Path::new("video.mp4");
        let no_ext_path = std::path::Path::new("video");

        assert_eq!(
            JobProcessor::determine_file_type(webm_path).unwrap(),
            MediaFileType::WebM
        );
        assert_eq!(
            JobProcessor::determine_file_type(mkv_path).unwrap(),
            MediaFileType::Mkv
        );
        assert!(JobProcessor::determine_file_type(mp4_path).is_err());
        assert!(JobProcessor::determine_file_type(no_ext_path).is_err());
    }

    #[tokio::test]
    async fn test_process_media_file_mkv() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create test file
        let mkv_file = media_root.join("video.mkv");
        fs::write(&mkv_file, "").unwrap();

        let queue = JobQueue::new(media_root.to_path_buf(), media_root.to_path_buf());
        queue.init().await.unwrap();

        let config = JobProcessorConfig::from_preset(None).unwrap();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.mkv");
        let result = processor
            .process_media_file(relative_path, MediaFileType::Mkv)
            .await
            .unwrap();

        assert!(matches!(result, JobProcessResult::Created));
    }

    #[tokio::test]
    async fn test_process_media_file_webm_with_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create test files
        let webm_file = media_root.join("video.webm");
        let vtt_file = media_root.join("video.vtt");
        fs::write(&webm_file, "").unwrap();
        fs::write(&vtt_file, "").unwrap();

        let queue = JobQueue::new(media_root.to_path_buf(), media_root.to_path_buf());
        queue.init().await.unwrap();

        let config = JobProcessorConfig::from_preset(None).unwrap();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.webm");
        let result = processor
            .process_media_file(relative_path, MediaFileType::WebM)
            .await
            .unwrap();

        assert!(matches!(result, JobProcessResult::Created));
    }

    #[tokio::test]
    async fn test_process_media_file_webm_missing_subtitle() {
        let temp_dir = TempDir::new().unwrap();
        let media_root = temp_dir.path();

        // Create only webm file (no .vtt)
        let webm_file = media_root.join("video.webm");
        fs::write(&webm_file, "").unwrap();

        let queue = JobQueue::new(media_root.to_path_buf(), media_root.to_path_buf());
        queue.init().await.unwrap();

        let config = JobProcessorConfig::from_preset(None).unwrap();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.webm");
        let result = processor
            .process_media_file(relative_path, MediaFileType::WebM)
            .await
            .unwrap();

        assert!(matches!(result, JobProcessResult::MissingSubtitle));
    }
}
