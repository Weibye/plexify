use anyhow::Result;
use std::path::Path;
use tracing::{info, warn};

use crate::job::{
    AudioAction, Job, MediaFileType, Operation, PostProcessingSettings, QualityPreset,
    QualitySettings, SubtitleAction,
};
use crate::paths::to_forward_slashes;
use crate::queue::JobQueue;

/// Shared job processing configuration.
///
/// Built once at the command line, so a command never reads the environment.
/// `Default` is what a job gets with no preset and no `FFMPEG_*` override.
#[derive(Default)]
pub struct JobProcessorConfig {
    pub quality_settings: QualitySettings,
    pub post_processing: PostProcessingSettings,
}

impl JobProcessorConfig {
    /// Create job processor config from preset or environment
    pub fn from_preset(preset: Option<QualityPreset>) -> Self {
        let quality_settings = match preset {
            Some(preset) => {
                info!("Using quality preset: {:?}", preset);
                QualitySettings::from_preset(preset)
            }
            None => {
                info!("Using quality settings from environment variables");
                QualitySettings::from_env()
            }
        };
        let post_processing = PostProcessingSettings::default();

        Self {
            quality_settings,
            post_processing,
        }
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

/// What to do to a file when no client was asked, decided from its extension.
///
/// `scan --target` asks what the file and the target hold instead. The
/// extension is not what makes these answers true, but they are measured on
/// this library:
///
/// - All 598 `.avi` files carry MPEG-4 or H.264 video both targets decode, and
///   MP3 or AC-3 audio. Only the container is wrong, so the video is copied.
/// - No `.avi` in this library carries a subtitle stream, and 586 of the 598
///   need a full re-encode for the Chromecast whatever happens to the audio.
///   So copying the audio and keeping a track list that is always empty costs
///   nothing measurable.
pub(crate) fn operation_for(file_type: &MediaFileType) -> Operation {
    match file_type {
        MediaFileType::Avi => Operation::Remux {
            audio: AudioAction::Copy,
            subtitles: SubtitleAction::Keep,
        },
        // No client was asked, so no channel cap is known.
        MediaFileType::WebM | MediaFileType::Mkv => Operation::Reencode { channels: None },
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
        operation: Operation,
    ) -> Result<JobProcessResult> {
        let job = Job::new(
            relative_path.to_path_buf(),
            file_type.clone(),
            operation,
            self.config.quality_settings.clone(),
            self.config.post_processing.clone(),
            self.media_root,
        );

        if job.output_exists(Some(self.media_root)) {
            return Ok(JobProcessResult::OutputExists);
        }

        if self.queue.job_exists(&job).await? {
            return Ok(JobProcessResult::AlreadyQueued);
        }

        if file_type == MediaFileType::WebM && !job.has_required_subtitle(Some(self.media_root))? {
            return Ok(JobProcessResult::MissingSubtitle);
        }

        self.queue.enqueue_job(&job).await?;

        Ok(JobProcessResult::Created)
    }

    /// Say what became of one file, and for a queued one, what its job does.
    pub fn log_result(
        &self,
        relative_path: &Path,
        operation: Operation,
        result: &JobProcessResult,
    ) {
        let path = to_forward_slashes(relative_path);
        match result {
            JobProcessResult::Created => {
                let work = match operation {
                    Operation::Remux { .. } => "remux",
                    Operation::Reencode { .. } => "re-encode",
                };
                info!("➕ Queueing {work} job for: {path:?}");
            }
            JobProcessResult::OutputExists => {
                info!("⏭️ Not queued, because its output already exists: {path:?}");
            }
            JobProcessResult::AlreadyQueued => {
                info!("⏭️ Not queued, because it already has a job: {path:?}");
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

        let config = JobProcessorConfig::from_preset(Some(QualityPreset::Quality));
        assert_eq!(config.quality_settings.ffmpeg_preset, "slow");
    }

    #[tokio::test]
    #[serial(env)]
    async fn test_job_processor_config_from_env() {
        clear_encoding_env();

        let config = JobProcessorConfig::from_preset(None);
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
        assert_eq!(
            operation_for(&MediaFileType::Mkv),
            Operation::Reencode { channels: None }
        );
        assert_eq!(
            operation_for(&MediaFileType::WebM),
            Operation::Reencode { channels: None }
        );
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

        let config = JobProcessorConfig::default();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.mkv");
        let result = processor
            .process_media_file(
                relative_path,
                MediaFileType::Mkv,
                Operation::Reencode { channels: None },
            )
            .await
            .unwrap();

        assert!(
            matches!(result, JobProcessResult::Created),
            "video.mkv is queued"
        );
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

        let config = JobProcessorConfig::default();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.webm");
        let result = processor
            .process_media_file(
                relative_path,
                MediaFileType::WebM,
                Operation::Reencode { channels: None },
            )
            .await
            .unwrap();

        assert!(
            matches!(result, JobProcessResult::Created),
            "video.webm is queued beside its video.vtt"
        );
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

        let config = JobProcessorConfig::default();
        let processor = JobProcessor::new(&queue, &config, media_root);

        let relative_path = std::path::Path::new("video.webm");
        let result = processor
            .process_media_file(
                relative_path,
                MediaFileType::WebM,
                Operation::Reencode { channels: None },
            )
            .await
            .unwrap();

        assert!(
            matches!(result, JobProcessResult::MissingSubtitle),
            "video.webm has no video.vtt"
        );
    }
}
