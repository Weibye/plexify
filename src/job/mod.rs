use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Represents a media file that needs to be transcoded
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub input_path: PathBuf,
    pub output_path: PathBuf,
    pub subtitle_path: Option<PathBuf>,
    pub file_type: MediaFileType,
    pub quality_settings: QualitySettings,
    pub post_processing: PostProcessingSettings,
}

/// Quality settings for video encoding
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QualitySettings {
    pub ffmpeg_preset: String,
    pub ffmpeg_crf: String,
    pub ffmpeg_audio_bitrate: String,
}

/// Predefined quality presets for different use cases
#[derive(Debug, Clone, PartialEq)]
pub enum QualityPreset {
    /// Fast encoding with good quality (veryfast/23/128k)
    Fast,
    /// Balanced encoding speed and quality (medium/20/192k)
    Balanced,
    /// High quality, slower encoding (slow/18/256k)
    Quality,
    /// Ultra-fast encoding for quick previews (ultrafast/28/96k)
    UltraFast,
    /// Archive quality for long-term storage (veryslow/15/320k)
    Archive,
}

/// Post-processing settings for what to do after conversion
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostProcessingSettings {
    pub disable_source_files: bool,
}

/// Supported media file types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MediaFileType {
    /// WebM file with external VTT subtitle
    WebM,
    /// MKV file with embedded subtitles
    Mkv,
}

impl Job {
    /// Create a new job for a media file with configuration
    pub fn new(
        input_path: PathBuf,
        file_type: MediaFileType,
        quality_settings: QualitySettings,
        post_processing: PostProcessingSettings,
    ) -> Self {
        let output_path = match file_type {
            MediaFileType::WebM => input_path.with_extension("mp4"),
            MediaFileType::Mkv => input_path.with_extension("mp4"),
        };

        let subtitle_path = match file_type {
            MediaFileType::WebM => Some(input_path.with_extension("vtt")),
            MediaFileType::Mkv => None, // MKV uses embedded subtitles
        };

        Self {
            id: Uuid::new_v4().to_string(),
            input_path,
            output_path,
            subtitle_path,
            file_type,
            quality_settings,
            post_processing,
        }
    }

    /// Get the job file name for the queue
    pub fn job_filename(&self) -> String {
        format!("{}.job", self.id)
    }

    /// Check if the output file already exists (works with both absolute and relative paths)
    pub fn output_exists(&self, media_root: Option<&Path>) -> bool {
        let output_path = if self.output_path.is_absolute() {
            self.output_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.output_path),
                None => self.output_path.clone(),
            }
        };
        output_path.exists()
    }

    /// For WebM files, check if the required subtitle file exists (works with both absolute and relative paths)
    pub fn has_required_subtitle(&self, media_root: Option<&Path>) -> Result<bool> {
        match self.file_type {
            MediaFileType::WebM => {
                if let Some(subtitle_path) = &self.subtitle_path {
                    let full_subtitle_path = if subtitle_path.is_absolute() {
                        subtitle_path.clone()
                    } else {
                        match media_root {
                            Some(root) => root.join(subtitle_path),
                            None => subtitle_path.clone(),
                        }
                    };
                    Ok(full_subtitle_path.exists())
                } else {
                    Err(anyhow!("WebM job should have subtitle path"))
                }
            }
            MediaFileType::Mkv => Ok(true), // MKV doesn't need external subtitles
        }
    }

    /// Get the full input path (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_input_path(&self, media_root: Option<&Path>) -> PathBuf {
        if self.input_path.is_absolute() {
            self.input_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.input_path),
                None => self.input_path.clone(),
            }
        }
    }

    /// Get the full output path (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_output_path(&self, media_root: Option<&Path>) -> PathBuf {
        if self.output_path.is_absolute() {
            self.output_path.clone()
        } else {
            match media_root {
                Some(root) => root.join(&self.output_path),
                None => self.output_path.clone(),
            }
        }
    }

    /// Get the full subtitle path if it exists (for absolute paths, returns as-is; for relative paths, joins with media_root)
    pub fn full_subtitle_path(&self, media_root: Option<&Path>) -> Option<PathBuf> {
        self.subtitle_path.as_ref().map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                match media_root {
                    Some(root) => root.join(path),
                    None => path.clone(),
                }
            }
        })
    }

    /// Create a job filename based on the source file (for compatibility)
    #[allow(dead_code)]
    pub fn job_filename_from_source(&self) -> String {
        let stem = self
            .input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        format!("{stem}.job")
    }
}

impl QualitySettings {
    /// Create quality settings from environment variables with defaults
    pub fn from_env() -> Self {
        use std::env;
        Self {
            ffmpeg_preset: env::var("FFMPEG_PRESET").unwrap_or_else(|_| "veryfast".to_string()),
            ffmpeg_crf: env::var("FFMPEG_CRF").unwrap_or_else(|_| "23".to_string()),
            ffmpeg_audio_bitrate: env::var("FFMPEG_AUDIO_BITRATE")
                .unwrap_or_else(|_| "128k".to_string()),
        }
    }

    /// Create quality settings from a preset, with optional environment variable overrides
    pub fn from_preset(preset: QualityPreset) -> Self {
        use std::env;
        let base = preset.to_quality_settings();

        Self {
            ffmpeg_preset: env::var("FFMPEG_PRESET").unwrap_or(base.ffmpeg_preset),
            ffmpeg_crf: env::var("FFMPEG_CRF").unwrap_or(base.ffmpeg_crf),
            ffmpeg_audio_bitrate: env::var("FFMPEG_AUDIO_BITRATE")
                .unwrap_or(base.ffmpeg_audio_bitrate),
        }
    }

    /// Create quality settings from a preset name string
    pub fn from_preset_name(preset_name: &str) -> Result<Self> {
        let preset = QualityPreset::from_name(preset_name)?;
        Ok(Self::from_preset(preset))
    }
}

impl QualityPreset {
    /// Convert preset to quality settings
    pub fn to_quality_settings(&self) -> QualitySettings {
        match self {
            QualityPreset::Fast => QualitySettings {
                ffmpeg_preset: "veryfast".to_string(),
                ffmpeg_crf: "23".to_string(),
                ffmpeg_audio_bitrate: "128k".to_string(),
            },
            QualityPreset::Balanced => QualitySettings {
                ffmpeg_preset: "medium".to_string(),
                ffmpeg_crf: "20".to_string(),
                ffmpeg_audio_bitrate: "192k".to_string(),
            },
            QualityPreset::Quality => QualitySettings {
                ffmpeg_preset: "slow".to_string(),
                ffmpeg_crf: "18".to_string(),
                ffmpeg_audio_bitrate: "256k".to_string(),
            },
            QualityPreset::UltraFast => QualitySettings {
                ffmpeg_preset: "ultrafast".to_string(),
                ffmpeg_crf: "28".to_string(),
                ffmpeg_audio_bitrate: "96k".to_string(),
            },
            QualityPreset::Archive => QualitySettings {
                ffmpeg_preset: "veryslow".to_string(),
                ffmpeg_crf: "15".to_string(),
                ffmpeg_audio_bitrate: "320k".to_string(),
            },
        }
    }

    /// Parse preset from string name
    pub fn from_name(name: &str) -> Result<Self> {
        match name.to_lowercase().as_str() {
            "fast" => Ok(QualityPreset::Fast),
            "balanced" => Ok(QualityPreset::Balanced),
            "quality" => Ok(QualityPreset::Quality),
            "ultrafast" => Ok(QualityPreset::UltraFast),
            "archive" => Ok(QualityPreset::Archive),
            _ => Err(anyhow!(
                "Unknown quality preset '{name}'. Available presets: fast, balanced, quality, ultrafast, archive"
            )),
        }
    }

    /// Get all available preset names
    #[allow(dead_code)]
    pub fn all_names() -> Vec<&'static str> {
        vec!["fast", "balanced", "quality", "ultrafast", "archive"]
    }

    /// Get the preset name as string
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        match self {
            QualityPreset::Fast => "fast",
            QualityPreset::Balanced => "balanced",
            QualityPreset::Quality => "quality",
            QualityPreset::UltraFast => "ultrafast",
            QualityPreset::Archive => "archive",
        }
    }
}

impl Default for QualitySettings {
    fn default() -> Self {
        Self {
            ffmpeg_preset: "veryfast".to_string(),
            ffmpeg_crf: "23".to_string(),
            ffmpeg_audio_bitrate: "128k".to_string(),
        }
    }
}

impl Default for PostProcessingSettings {
    fn default() -> Self {
        Self {
            disable_source_files: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_webm_job_creation() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let job = Job::new(
            PathBuf::from("video.webm"),
            MediaFileType::WebM,
            quality,
            post_processing,
        );
        assert_eq!(job.input_path, PathBuf::from("video.webm"));
        assert_eq!(job.file_type, MediaFileType::WebM);
        assert_eq!(job.output_path, PathBuf::from("video.mp4"));
        assert_eq!(job.subtitle_path, Some(PathBuf::from("video.vtt")));
    }

    #[test]
    fn test_mkv_job_creation() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let job = Job::new(
            PathBuf::from("video.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
        );
        assert_eq!(job.input_path, PathBuf::from("video.mkv"));
        assert_eq!(job.file_type, MediaFileType::Mkv);
        assert_eq!(job.output_path, PathBuf::from("video.mp4"));
        assert_eq!(job.subtitle_path, None);
    }

    #[test]
    fn test_quality_settings_from_env() {
        std::env::set_var("FFMPEG_PRESET", "fast");
        std::env::set_var("FFMPEG_CRF", "20");
        std::env::set_var("FFMPEG_AUDIO_BITRATE", "192k");

        let quality = QualitySettings::from_env();
        assert_eq!(quality.ffmpeg_preset, "fast");
        assert_eq!(quality.ffmpeg_crf, "20");
        assert_eq!(quality.ffmpeg_audio_bitrate, "192k");

        // Clean up
        std::env::remove_var("FFMPEG_PRESET");
        std::env::remove_var("FFMPEG_CRF");
        std::env::remove_var("FFMPEG_AUDIO_BITRATE");
    }

    #[test]
    fn test_absolute_paths() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let job = Job::new(
            PathBuf::from("/absolute/path/video.webm"),
            MediaFileType::WebM,
            quality,
            post_processing,
        );

        // Test that absolute paths work without media_root
        assert_eq!(
            job.full_input_path(None),
            PathBuf::from("/absolute/path/video.webm")
        );
        assert_eq!(
            job.full_output_path(None),
            PathBuf::from("/absolute/path/video.mp4")
        );
        assert_eq!(
            job.full_subtitle_path(None),
            Some(PathBuf::from("/absolute/path/video.vtt"))
        );

        // Test that absolute paths ignore media_root
        let media_root = PathBuf::from("/different/root");
        assert_eq!(
            job.full_input_path(Some(&media_root)),
            PathBuf::from("/absolute/path/video.webm")
        );
        assert_eq!(
            job.full_output_path(Some(&media_root)),
            PathBuf::from("/absolute/path/video.mp4")
        );
    }

    #[test]
    fn test_relative_paths_with_media_root() {
        let quality = QualitySettings::default();
        let post_processing = PostProcessingSettings::default();
        let job = Job::new(
            PathBuf::from("relative/video.mkv"),
            MediaFileType::Mkv,
            quality,
            post_processing,
        );

        let media_root = PathBuf::from("/media/root");

        // Test that relative paths are resolved with media_root
        assert_eq!(
            job.full_input_path(Some(&media_root)),
            PathBuf::from("/media/root/relative/video.mkv")
        );
        assert_eq!(
            job.full_output_path(Some(&media_root)),
            PathBuf::from("/media/root/relative/video.mp4")
        );
        assert_eq!(job.full_subtitle_path(Some(&media_root)), None); // MKV has no external subtitles

        // Test that relative paths work without media_root (use as-is)
        assert_eq!(
            job.full_input_path(None),
            PathBuf::from("relative/video.mkv")
        );
        assert_eq!(
            job.full_output_path(None),
            PathBuf::from("relative/video.mp4")
        );
    }

    #[test]
    fn test_quality_presets() {
        // Test Fast preset
        let fast = QualityPreset::Fast.to_quality_settings();
        assert_eq!(fast.ffmpeg_preset, "veryfast");
        assert_eq!(fast.ffmpeg_crf, "23");
        assert_eq!(fast.ffmpeg_audio_bitrate, "128k");

        // Test Quality preset
        let quality = QualityPreset::Quality.to_quality_settings();
        assert_eq!(quality.ffmpeg_preset, "slow");
        assert_eq!(quality.ffmpeg_crf, "18");
        assert_eq!(quality.ffmpeg_audio_bitrate, "256k");

        // Test Balanced preset
        let balanced = QualityPreset::Balanced.to_quality_settings();
        assert_eq!(balanced.ffmpeg_preset, "medium");
        assert_eq!(balanced.ffmpeg_crf, "20");
        assert_eq!(balanced.ffmpeg_audio_bitrate, "192k");
    }

    #[test]
    fn test_preset_from_name() {
        assert_eq!(
            QualityPreset::from_name("fast").unwrap(),
            QualityPreset::Fast
        );
        assert_eq!(
            QualityPreset::from_name("QUALITY").unwrap(),
            QualityPreset::Quality
        );
        assert_eq!(
            QualityPreset::from_name("Balanced").unwrap(),
            QualityPreset::Balanced
        );

        // Test invalid preset name
        assert!(QualityPreset::from_name("invalid").is_err());
    }

    #[test]
    fn test_quality_settings_from_preset() {
        let settings = QualitySettings::from_preset(QualityPreset::Quality);
        assert_eq!(settings.ffmpeg_preset, "slow");
        assert_eq!(settings.ffmpeg_crf, "18");
        assert_eq!(settings.ffmpeg_audio_bitrate, "256k");
    }

    #[test]
    fn test_quality_settings_from_preset_name() {
        let settings = QualitySettings::from_preset_name("balanced").unwrap();
        assert_eq!(settings.ffmpeg_preset, "medium");
        assert_eq!(settings.ffmpeg_crf, "20");
        assert_eq!(settings.ffmpeg_audio_bitrate, "192k");

        // Test invalid name
        assert!(QualitySettings::from_preset_name("invalid").is_err());
    }

    #[test]
    fn test_preset_with_env_override() {
        // Set environment variables
        std::env::set_var("FFMPEG_PRESET", "custom");
        std::env::set_var("FFMPEG_CRF", "25");

        let settings = QualitySettings::from_preset(QualityPreset::Quality);
        assert_eq!(settings.ffmpeg_preset, "custom"); // Overridden by env
        assert_eq!(settings.ffmpeg_crf, "25"); // Overridden by env
        assert_eq!(settings.ffmpeg_audio_bitrate, "256k"); // From preset

        // Clean up
        std::env::remove_var("FFMPEG_PRESET");
        std::env::remove_var("FFMPEG_CRF");
    }

    #[test]
    fn test_job_serialization() {
        let quality = QualitySettings {
            ffmpeg_preset: "medium".to_string(),
            ffmpeg_crf: "18".to_string(),
            ffmpeg_audio_bitrate: "256k".to_string(),
        };
        let post_processing = PostProcessingSettings {
            disable_source_files: false,
        };
        let job = Job::new(
            PathBuf::from("test.webm"),
            MediaFileType::WebM,
            quality.clone(),
            post_processing.clone(),
        );

        // Test JSON serialization/deserialization
        let json = serde_json::to_string(&job).unwrap();
        let deserialized: Job = serde_json::from_str(&json).unwrap();

        assert_eq!(job.input_path, deserialized.input_path);
        assert_eq!(job.output_path, deserialized.output_path);
        assert_eq!(job.subtitle_path, deserialized.subtitle_path);
        assert_eq!(job.file_type, deserialized.file_type);
        assert_eq!(
            job.quality_settings.ffmpeg_preset,
            deserialized.quality_settings.ffmpeg_preset
        );
        assert_eq!(
            job.post_processing.disable_source_files,
            deserialized.post_processing.disable_source_files
        );
    }
}
