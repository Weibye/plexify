use anyhow::{Result, anyhow};
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{info, error, debug};

use crate::config::Config;
use crate::job::{Job, MediaFileType};

/// FFmpeg wrapper for media transcoding
pub struct FFmpegProcessor {
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
    pub async fn process_job(&self, job: &Job, media_root: &Path) -> Result<()> {
        let input_path = media_root.join(&job.relative_path);
        let output_path = media_root.join(job.output_path());

        info!("🚀 Starting conversion for: {:?}", input_path);

        let mut cmd = if self.background_mode {
            let mut c = Command::new("nice");
            c.args(["-n", "19"]);
            c.arg("ffmpeg");
            c
        } else {
            Command::new("ffmpeg")
        };

        // Add common FFmpeg flags
        cmd.args([
            "-fflags", "+genpts",
            "-avoid_negative_ts", "make_zero",
        ]);

        // Add format-specific flags and inputs
        match job.file_type {
            MediaFileType::WebM => {
                if let Some(subtitle_path) = job.subtitle_path() {
                    let vtt_path = media_root.join(subtitle_path);
                    
                    // Check if subtitle file exists
                    if !vtt_path.exists() {
                        return Err(anyhow!("Required subtitle file not found: {:?}", vtt_path));
                    }

                    cmd.args(["-i", input_path.to_str().unwrap()]);
                    cmd.args(["-i", vtt_path.to_str().unwrap()]);
                    cmd.args(["-map", "0:v:0", "-map", "0:a:0", "-map", "1:s:0"]);
                } else {
                    return Err(anyhow!("WebM job missing subtitle path"));
                }
            }
            MediaFileType::MKV => {
                cmd.args(["-fix_sub_duration"]);
                cmd.args(["-i", input_path.to_str().unwrap()]);
                cmd.args(["-map", "0:v:0", "-map", "0:a:0", "-map", "0:s:0"]);
            }
        }

        // Add encoding settings
        cmd.args([
            "-c:v", "libx264",
            "-preset", &self.config.ffmpeg_preset,
            "-crf", &self.config.ffmpeg_crf,
            "-c:a", "aac",
            "-b:a", &self.config.ffmpeg_audio_bitrate,
            "-c:s", "mov_text",
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
            return Err(anyhow!("FFmpeg conversion failed: {}", stderr));
        }

        info!("✅ Conversion successful: {:?} -> {:?}", input_path, output_path);
        Ok(())
    }

    /// Rename original files to .disabled after successful conversion
    pub async fn disable_source_files(&self, job: &Job, media_root: &Path) -> Result<()> {
        let input_path = media_root.join(&job.relative_path);
        let disabled_input = input_path.with_extension(
            format!("{}.disabled", input_path.extension().unwrap_or_default().to_str().unwrap_or(""))
        );

        // Rename input file
        tokio::fs::rename(&input_path, &disabled_input).await?;
        debug!("Renamed input file: {:?} -> {:?}", input_path, disabled_input);

        // Rename subtitle file if it exists (WebM)
        if let Some(subtitle_path) = job.subtitle_path() {
            let vtt_path = media_root.join(subtitle_path);
            if vtt_path.exists() {
                let disabled_vtt = vtt_path.with_extension("vtt.disabled");
                tokio::fs::rename(&vtt_path, &disabled_vtt).await?;
                debug!("Renamed subtitle file: {:?} -> {:?}", vtt_path, disabled_vtt);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use crate::job::{Job, MediaFileType};

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
}