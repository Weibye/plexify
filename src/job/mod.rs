use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Represents a media file that needs to be transcoded
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: String,
    pub relative_path: PathBuf,
    pub file_type: MediaFileType,
}

/// Supported media file types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum MediaFileType {
    /// WebM file with external VTT subtitle
    WebM,
    /// MKV file with embedded subtitles
    MKV,
}

impl Job {
    /// Create a new job for a media file
    pub fn new(relative_path: PathBuf, file_type: MediaFileType) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            relative_path,
            file_type,
        }
    }

    /// Get the output file path (same as input but with .mp4 extension)
    pub fn output_path(&self) -> PathBuf {
        match self.file_type {
            MediaFileType::WebM => self.relative_path.with_extension("mp4"),
            MediaFileType::MKV => self.relative_path.with_extension("mp4"),
        }
    }

    /// Get the subtitle file path for WebM files
    pub fn subtitle_path(&self) -> Option<PathBuf> {
        match self.file_type {
            MediaFileType::WebM => Some(self.relative_path.with_extension("vtt")),
            MediaFileType::MKV => None, // MKV uses embedded subtitles
        }
    }

    /// Get the job file name for the queue
    pub fn job_filename(&self) -> String {
        format!("{}.job", self.id)
    }

    /// Check if the output file already exists
    pub fn output_exists(&self, media_root: &Path) -> bool {
        media_root.join(self.output_path()).exists()
    }

    /// For WebM files, check if the required subtitle file exists
    pub fn has_required_subtitle(&self, media_root: &Path) -> Result<bool> {
        match self.file_type {
            MediaFileType::WebM => {
                if let Some(subtitle_path) = self.subtitle_path() {
                    Ok(media_root.join(subtitle_path).exists())
                } else {
                    Err(anyhow!("WebM job should have subtitle path"))
                }
            }
            MediaFileType::MKV => Ok(true), // MKV doesn't need external subtitles
        }
    }

    /// Create a job filename based on the source file (for compatibility)
    pub fn job_filename_from_source(&self) -> String {
        let stem = self.relative_path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        format!("{}.job", stem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_webm_job_creation() {
        let job = Job::new(PathBuf::from("video.webm"), MediaFileType::WebM);
        assert_eq!(job.relative_path, PathBuf::from("video.webm"));
        assert_eq!(job.file_type, MediaFileType::WebM);
        assert_eq!(job.output_path(), PathBuf::from("video.mp4"));
        assert_eq!(job.subtitle_path(), Some(PathBuf::from("video.vtt")));
    }

    #[test]
    fn test_mkv_job_creation() {
        let job = Job::new(PathBuf::from("video.mkv"), MediaFileType::MKV);
        assert_eq!(job.relative_path, PathBuf::from("video.mkv"));
        assert_eq!(job.file_type, MediaFileType::MKV);
        assert_eq!(job.output_path(), PathBuf::from("video.mp4"));
        assert_eq!(job.subtitle_path(), None);
    }
}