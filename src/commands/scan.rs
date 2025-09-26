use anyhow::{Result, anyhow};
use std::path::PathBuf;
use tracing::{info, warn, debug};
use walkdir::WalkDir;

use crate::job::{Job, MediaFileType};
use crate::queue::JobQueue;

/// Command to scan a directory for media files and create jobs
pub struct ScanCommand {
    media_root: PathBuf,
}

impl ScanCommand {
    pub fn new(media_root: PathBuf) -> Self {
        Self { media_root }
    }

    pub async fn execute(&self) -> Result<()> {
        if !self.media_root.exists() {
            return Err(anyhow!("Media directory does not exist: {:?}", self.media_root));
        }

        if !self.media_root.is_dir() {
            return Err(anyhow!("Path is not a directory: {:?}", self.media_root));
        }

        info!("🔎 Scanning directory: {:?}", self.media_root);

        let queue = JobQueue::new(self.media_root.clone());
        queue.init().await?;

        let mut webm_files = Vec::new();
        let mut mkv_files = Vec::new();

        // Walk through the directory to find media files
        for entry in WalkDir::new(&self.media_root)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                if let Some(extension) = path.extension() {
                    let ext_str = extension.to_string_lossy().to_lowercase();
                    match ext_str.as_str() {
                        "webm" => {
                            if let Ok(relative_path) = path.strip_prefix(&self.media_root) {
                                webm_files.push(relative_path.to_path_buf());
                            }
                        }
                        "mkv" => {
                            if let Ok(relative_path) = path.strip_prefix(&self.media_root) {
                                mkv_files.push(relative_path.to_path_buf());
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        info!("Found {} .webm files and {} .mkv files. Now creating jobs...", 
              webm_files.len(), mkv_files.len());

        let mut job_count = 0;

        // Process WebM files (require VTT subtitles)
        for webm_path in webm_files {
            let job = Job::new(webm_path.clone(), MediaFileType::WebM);

            // Check if output already exists
            if job.output_exists(&self.media_root) {
                debug!("Output already exists for: {:?}", webm_path);
                continue;
            }

            // Check if job already exists in queue
            if queue.job_exists(&job).await? {
                debug!("Job already exists for: {:?}", webm_path);
                continue;
            }

            // Check if required subtitle file exists
            if !job.has_required_subtitle(&self.media_root)? {
                warn!("⚠️ SKIPPING: Missing subtitle file for '{:?}'", webm_path);
                continue;
            }

            // Create the job
            queue.enqueue_job(&job).await?;
            info!("➕ Queueing job for: {:?}", webm_path);
            job_count += 1;
        }

        // Process MKV files (embedded subtitles)
        for mkv_path in mkv_files {
            let job = Job::new(mkv_path.clone(), MediaFileType::MKV);

            // Check if output already exists
            if job.output_exists(&self.media_root) {
                debug!("Output already exists for: {:?}", mkv_path);
                continue;
            }

            // Check if job already exists in queue
            if queue.job_exists(&job).await? {
                debug!("Job already exists for: {:?}", mkv_path);
                continue;
            }

            // Create the job
            queue.enqueue_job(&job).await?;
            info!("➕ Queueing job for: {:?} (embedded subs assumed)", mkv_path);
            job_count += 1;
        }

        info!("✅ Scan complete. Added {} new jobs to the queue.", job_count);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    #[tokio::test]
    async fn test_scan_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let scan_cmd = ScanCommand::new(temp_dir.path().to_path_buf());
        
        let result = scan_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_scan_nonexistent_directory() {
        let scan_cmd = ScanCommand::new(PathBuf::from("/nonexistent/path"));
        
        let result = scan_cmd.execute().await;
        assert!(result.is_err());
    }
}