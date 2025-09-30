use anyhow::{anyhow, Result};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, error, info};

use crate::config::Config;
use crate::job::{Job, MediaFileType};

/// FFmpeg wrapper for media transcoding
pub struct FFmpegProcessor {
    #[allow(dead_code)]
    config: Config,
    background_mode: bool,
}

impl FFmpegProcessor {
    pub fn new(config: Config, background_mode: bool) -> Self {
        Self {
            config,
            background_mode,
        }
    }

    /// Process a job using FFmpeg
    pub async fn process_job(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        work_folder: Option<&Path>,
    ) -> Result<()> {
        let input_path = job.full_input_path(media_root);
        let output_path = if let Some(work_folder) = work_folder {
            job.work_folder_output_path(work_folder)
        } else {
            job.full_output_path(media_root)
        };

        info!("🚀 Starting conversion for: {:?}", input_path);

        // Ensure input file exists
        if !input_path.exists() {
            return Err(anyhow!("Input file does not exist: {input_path:?}"));
        }

        // Create output directory if it doesn't exist
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut cmd = if self.background_mode {
            let mut c = Command::new("nice");
            c.args(["-n", "19"]);
            c.arg("ffmpeg");
            c
        } else {
            Command::new("ffmpeg")
        };

        // Add common FFmpeg flags
        cmd.args(["-fflags", "+genpts", "-avoid_negative_ts", "make_zero"]);

        // Add format-specific flags and inputs
        match job.file_type {
            MediaFileType::WebM => {
                if let Some(vtt_path) = job.full_subtitle_path(media_root) {
                    // Check if subtitle file exists
                    if !vtt_path.exists() {
                        return Err(anyhow!("Required subtitle file not found: {vtt_path:?}"));
                    }

                    cmd.args(["-i", input_path.to_str().unwrap()]);
                    cmd.args(["-i", vtt_path.to_str().unwrap()]);
                    cmd.args(["-map", "0:v:0", "-map", "0:a:0", "-map", "1:s:0"]);
                } else {
                    return Err(anyhow!("WebM job missing subtitle path"));
                }
            }
            MediaFileType::Mkv => {
                cmd.args(["-fix_sub_duration"]);
                cmd.args(["-i", input_path.to_str().unwrap()]);
                cmd.args(["-map", "0:v:0", "-map", "0:a:0", "-map", "0:s:0"]);
            }
        }

        // Add encoding settings using job's quality settings
        cmd.args([
            "-c:v",
            "libx264",
            "-preset",
            &job.quality_settings.ffmpeg_preset,
            "-crf",
            &job.quality_settings.ffmpeg_crf,
            "-c:a",
            "aac",
            "-b:a",
            &job.quality_settings.ffmpeg_audio_bitrate,
            "-c:s",
            "mov_text",
            "-y", // Overwrite output files
        ]);

        cmd.arg(output_path.to_str().unwrap());

        // Set up stdio
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        debug!("Executing FFmpeg command: {:?}", cmd);

        // Execute FFmpeg
        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            error!("FFmpeg failed: {}", stderr);
            return Err(anyhow!("FFmpeg conversion failed: {stderr}"));
        }

        info!(
            "✅ Conversion successful: {:?} -> {:?}",
            input_path, output_path
        );
        Ok(())
    }

    /// Move completed file from work folder to media folder
    pub async fn move_to_destination(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        work_folder: &Path,
    ) -> Result<()> {
        let work_output_path = job.work_folder_output_path(work_folder);
        let final_output_path = job.full_output_path(media_root);

        // Ensure the work folder output file exists
        if !work_output_path.exists() {
            return Err(anyhow!(
                "Work folder output file does not exist: {work_output_path:?}"
            ));
        }

        // Create final output directory if it doesn't exist
        if let Some(parent) = final_output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Move the file from work folder to final location
        tokio::fs::rename(&work_output_path, &final_output_path).await?;

        info!(
            "📁 Moved completed file: {:?} -> {:?}",
            work_output_path, final_output_path
        );

        Ok(())
    }
    pub async fn disable_source_files(&self, job: &Job, media_root: Option<&Path>) -> Result<()> {
        let input_path = job.full_input_path(media_root);
        let disabled_input = input_path.with_extension(format!(
            "{}.disabled",
            input_path
                .extension()
                .unwrap_or_default()
                .to_str()
                .unwrap_or("")
        ));

        // Rename input file
        tokio::fs::rename(&input_path, &disabled_input).await?;
        debug!(
            "Renamed input file: {:?} -> {:?}",
            input_path, disabled_input
        );

        // Rename subtitle file if it exists (WebM)
        if let Some(vtt_path) = job.full_subtitle_path(media_root) {
            if vtt_path.exists() {
                let disabled_vtt = vtt_path.with_extension("vtt.disabled");
                tokio::fs::rename(&vtt_path, &disabled_vtt).await?;
                debug!(
                    "Renamed subtitle file: {:?} -> {:?}",
                    vtt_path, disabled_vtt
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Job, MediaFileType, PostProcessingSettings, QualitySettings};
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_ffmpeg_processor_creation() {
        let config = Config::default();
        let processor = FFmpegProcessor::new(config, false);
        assert!(!processor.background_mode);
    }

    #[tokio::test]
    async fn test_background_mode() {
        let config = Config::default();
        let processor = FFmpegProcessor::new(config, true);
        assert!(processor.background_mode);
    }

    #[tokio::test]
    async fn test_work_folder_output_path_generation() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
        );

        let work_output_path = job.work_folder_output_path(work_folder);

        // Verify the path structure
        assert!(work_output_path.starts_with(work_folder));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&job.id));
        assert!(work_output_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with("test.mp4"));
    }

    #[tokio::test]
    async fn test_move_to_destination() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path().join("work");
        let media_folder = temp_dir.path().join("media");

        tokio::fs::create_dir_all(&work_folder).await.unwrap();
        tokio::fs::create_dir_all(&media_folder).await.unwrap();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
        );

        // Create a dummy file in the work folder
        let work_output_path = job.work_folder_output_path(&work_folder);
        tokio::fs::write(&work_output_path, "test content")
            .await
            .unwrap();

        let config = Config::default();
        let processor = FFmpegProcessor::new(config, false);

        // Move the file
        processor
            .move_to_destination(&job, Some(&media_folder), &work_folder)
            .await
            .unwrap();

        // Verify the file was moved
        assert!(!work_output_path.exists());
        let final_path = job.full_output_path(Some(&media_folder));
        assert!(final_path.exists());

        let content = tokio::fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, "test content");
    }
}
