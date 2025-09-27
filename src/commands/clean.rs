use anyhow::{anyhow, Result};
use std::path::PathBuf;
use tracing::info;

use crate::queue::JobQueue;

/// Command to clean up temporary files and directories
pub struct CleanCommand {
    media_root: PathBuf,
}

impl CleanCommand {
    pub fn new(media_root: PathBuf) -> Self {
        Self { media_root }
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

        info!("🧹 Cleaning up temporary files...");

        let queue = JobQueue::new(self.media_root.clone());
        queue.clean().await?;

        // Also clean up worker log if it exists
        let worker_log = self.media_root.join("_worker.log");
        if worker_log.exists() {
            tokio::fs::remove_file(&worker_log).await?;
            info!("Removed worker log: {:?}", worker_log);
        }

        info!("✅ Cleanup complete.");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_clean_empty_directory() {
        let temp_dir = TempDir::new().unwrap();
        let clean_cmd = CleanCommand::new(temp_dir.path().to_path_buf());

        let result = clean_cmd.execute().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_clean_nonexistent_directory() {
        let clean_cmd = CleanCommand::new(PathBuf::from("/nonexistent/path"));

        let result = clean_cmd.execute().await;
        assert!(result.is_err());
    }
}
