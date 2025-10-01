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

    /// Process a job using FFmpeg, with support for resuming from partial progress
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

        // Check if we have a partial file to resume from
        let partial_file_exists = job
            .progress
            .partial_output_path
            .as_ref()
            .map(|p| p.exists())
            .unwrap_or(false);

        if partial_file_exists && job.progress.partial_duration_seconds.is_some() {
            let resume_time = job.progress.partial_duration_seconds.unwrap();
            info!(
                "🔄 Resuming conversion from {:.1}s for: {:?}",
                resume_time, input_path
            );

            // Resume by creating continuation segment and concatenating
            return self
                .resume_transcoding(job, media_root, &output_path, resume_time)
                .await;
        } else {
            info!("🚀 Starting conversion for: {:?}", input_path);

            // Normal full transcoding
            return self.transcode_full(job, media_root, &output_path).await;
        }
    }

    /// Perform complete transcoding from start
    async fn transcode_full(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        output_path: &Path,
    ) -> Result<()> {
        let input_path = job.full_input_path(media_root);

        // Ensure input file exists
        if !input_path.exists() {
            return Err(anyhow!("Input file does not exist: {input_path:?}"));
        }

        // Create output directory if it doesn't exist
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut cmd = self.build_base_command();

        // Add input-specific options
        self.add_input_options(&mut cmd, job, media_root)?;

        // Add encoding options
        self.add_encoding_options(&mut cmd, job);

        // Normal overwrite
        cmd.args(["-y"]);
        cmd.arg(output_path.to_str().unwrap());

        self.execute_ffmpeg_command(cmd, &input_path, output_path)
            .await
    }

    /// Resume transcoding by creating continuation segment and concatenating with partial file
    async fn resume_transcoding(
        &self,
        job: &Job,
        media_root: Option<&Path>,
        output_path: &Path,
        resume_from_seconds: f64,
    ) -> Result<()> {
        let input_path = job.full_input_path(media_root);
        let partial_path = job.progress.partial_output_path.as_ref().unwrap();

        // Create a temporary file for the continuation segment
        let continuation_path = output_path.with_extension("continuation.mp4");

        // Step 1: Create continuation segment starting from resume point
        let mut cmd = self.build_base_command();

        // Seek to resume position in input
        cmd.args(["-ss", &resume_from_seconds.to_string()]);

        // Add input-specific options
        self.add_input_options(&mut cmd, job, media_root)?;

        // Add encoding options
        self.add_encoding_options(&mut cmd, job);

        // Output continuation segment
        cmd.args(["-y"]);
        cmd.arg(continuation_path.to_str().unwrap());

        info!(
            "Creating continuation segment from {:.1}s",
            resume_from_seconds
        );
        self.execute_ffmpeg_command(cmd, &input_path, &continuation_path)
            .await?;

        // Step 2: Concatenate partial file with continuation segment
        info!("Concatenating partial file with continuation segment");
        self.concatenate_segments(partial_path, &continuation_path, output_path)
            .await?;

        // Clean up temporary continuation file
        let _ = tokio::fs::remove_file(&continuation_path).await;

        Ok(())
    }

    /// Concatenate two video segments into final output
    async fn concatenate_segments(
        &self,
        partial_path: &Path,
        continuation_path: &Path,
        output_path: &Path,
    ) -> Result<()> {
        // Create concat list file
        let concat_list_path = output_path.with_extension("concat.txt");
        let concat_content = format!(
            "file '{}'\nfile '{}'",
            partial_path.to_str().unwrap(),
            continuation_path.to_str().unwrap()
        );
        tokio::fs::write(&concat_list_path, concat_content).await?;

        let mut cmd = self.build_base_command();
        cmd.args([
            "-f",
            "concat",
            "-safe",
            "0",
            "-i",
            concat_list_path.to_str().unwrap(),
            "-c",
            "copy", // Copy streams without re-encoding
            "-y",
        ]);
        cmd.arg(output_path.to_str().unwrap());

        let result = self
            .execute_ffmpeg_command(cmd, partial_path, output_path)
            .await;

        // Clean up concat list file
        let _ = tokio::fs::remove_file(&concat_list_path).await;

        result
    }

    /// Build base FFmpeg command with common options
    fn build_base_command(&self) -> Command {
        if self.background_mode {
            let mut c = Command::new("nice");
            c.args(["-n", "19"]);
            c.arg("ffmpeg");
            c
        } else {
            Command::new("ffmpeg")
        }
    }

    /// Add input-specific options to FFmpeg command
    fn add_input_options(
        &self,
        cmd: &mut Command,
        job: &Job,
        media_root: Option<&Path>,
    ) -> Result<()> {
        let input_path = job.full_input_path(media_root);

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

        Ok(())
    }

    /// Add encoding options to FFmpeg command
    fn add_encoding_options(&self, cmd: &mut Command, job: &Job) {
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
        ]);
    }

    /// Execute FFmpeg command and handle result
    async fn execute_ffmpeg_command(
        &self,
        mut cmd: Command,
        input_path: &Path,
        output_path: &Path,
    ) -> Result<()> {
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

    /// Get duration of a media file in seconds using ffprobe
    pub async fn get_duration(&self, file_path: &Path) -> Result<f64> {
        if !file_path.exists() {
            return Err(anyhow!("File does not exist: {file_path:?}"));
        }

        let mut cmd = Command::new("ffprobe");
        cmd.args([
            "-v",
            "quiet",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            file_path.to_str().unwrap(),
        ]);

        let output = cmd.output().await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("ffprobe failed: {stderr}"));
        }

        let duration_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
        duration_str
            .parse::<f64>()
            .map_err(|e| anyhow!("Failed to parse duration '{}': {}", duration_str, e))
    }

    /// Check if a job has a partially transcoded file and update progress
    pub async fn detect_partial_progress(&self, job: &mut Job, work_folder: &Path) -> Result<bool> {
        let partial_path = job.work_folder_output_path(work_folder);

        if !partial_path.exists() {
            return Ok(false);
        }

        // Get duration of partial file
        match self.get_duration(&partial_path).await {
            Ok(duration) => {
                if duration > 0.0 {
                    info!(
                        "🔄 Found partial transcoding: {:.1}s completed in {:?}",
                        duration, partial_path
                    );
                    job.progress.started = true;
                    job.progress.partial_duration_seconds = Some(duration);
                    job.progress.partial_output_path = Some(partial_path);
                    Ok(true)
                } else {
                    // Empty or invalid file, remove it
                    let _ = tokio::fs::remove_file(&partial_path).await;
                    Ok(false)
                }
            }
            Err(_) => {
                // Invalid partial file, remove it
                let _ = tokio::fs::remove_file(&partial_path).await;
                Ok(false)
            }
        }
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
        let media_root = temp_dir.path();
        let job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
            media_root,
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
            &media_folder,
        );

        // Create a dummy file in the work folder
        let work_output_path = job.work_folder_output_path(&work_folder);
        tokio::fs::write(&work_output_path, "test content")
            .await
            .unwrap();

        let config = Config::default();
        let processor = FFmpegProcessor::new(config, false);

        // Move the file - since job now has absolute paths, pass None for media_root
        processor
            .move_to_destination(&job, None, &work_folder)
            .await
            .unwrap();

        // Verify the file was moved
        assert!(!work_output_path.exists());
        let final_path = job.full_output_path(None);
        assert!(final_path.exists());

        let content = tokio::fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, "test content");
    }

    #[tokio::test]
    async fn test_detect_partial_progress() {
        let temp_dir = TempDir::new().unwrap();
        let work_folder = temp_dir.path();

        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let media_root = temp_dir.path();
        let mut job = Job::new(
            PathBuf::from("test.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
            media_root,
        );

        let config = Config::default();
        let processor = FFmpegProcessor::new(config, false);

        // Initially no partial progress
        let has_partial = processor
            .detect_partial_progress(&mut job, work_folder)
            .await
            .unwrap();
        assert!(!has_partial);
        assert!(!job.progress.started);

        // Create a dummy partial file (since we don't have ffprobe in CI)
        let partial_path = job.work_folder_output_path(work_folder);
        tokio::fs::write(&partial_path, "partial content")
            .await
            .unwrap();

        // Test again - this will fail because we don't have ffprobe, but it should handle the error gracefully
        let has_partial = processor
            .detect_partial_progress(&mut job, work_folder)
            .await
            .unwrap();

        // Since ffprobe will fail, it should remove the invalid file and return false
        assert!(!has_partial);
        assert!(!partial_path.exists());
    }
}
