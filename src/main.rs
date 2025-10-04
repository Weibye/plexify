//! # Plexify - Media Transcoding CLI
//!
//! A simple, distributed media transcoding CLI tool that converts .webm and .mkv files
//! to .mp4 format with subtitle support, optimized for Plex media servers.
//!
//! ## Features
//!
//! - **Distributed Processing**: Queue-based system allows multiple workers to process jobs concurrently
//! - **Subtitle Support**: Handles external .vtt subtitles for .webm files and embedded subtitles for .mkv files
//! - **Background Processing**: Run workers in low-priority background mode
//! - **Configurable**: Customizable FFmpeg settings via environment variables
//! - **Atomic Job Processing**: Race condition-free job claiming for multiple workers
//! - **Signal Handling**: Graceful shutdown on SIGINT/SIGTERM
//!
//! ## Usage
//!
//! ```bash
//! # Scan a directory for media files
//! plexify scan /path/to/media
//!
//! # Process jobs from the queue
//! plexify work /path/to/media
//!
//! # Clean up temporary files
//! plexify clean /path/to/media
//!
//! # Validate Plex naming scheme conformity
//! plexify validate /path/to/media
//! ```

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod config;
mod ffmpeg;
mod ignore;
mod job;
mod queue;
mod worker;

use commands::{
    add::AddCommand, clean::CleanCommand, scan::ScanCommand, validate::ValidateCommand,
    work::WorkCommand,
};
use plexify::JobPriority;

/// Plexify - A simple, distributed media transcoding CLI
#[derive(Parser)]
#[command(
    name = "plexify",
    about = "A simple, distributed media transcoding CLI tool",
    long_about = "Converts .webm and .mkv files to .mp4 format with subtitle support, optimized for Plex media servers.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Available commands
#[derive(Subcommand)]
enum Commands {
    /// Create a transcoding job for an individual media file
    Add {
        /// Path to the media file to process
        file: PathBuf,
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
        /// Quality preset for encoding. Available: fast, balanced, quality, ultrafast, archive
        #[arg(long, short = 'p')]
        preset: Option<String>,
    },
    /// Scan a directory for media files and create transcoding jobs
    Scan {
        /// Path to the media directory to scan
        path: PathBuf,
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
        /// Quality preset for encoding. Available: fast, balanced, quality, ultrafast, archive
        #[arg(long, short = 'p')]
        preset: Option<String>,
    },
    /// Process jobs from the queue
    Work {
        /// Path to the media directory containing the media files
        path: PathBuf,
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
        /// Run worker in background with low priority
        #[arg(long, short)]
        background: bool,
        /// Job prioritization method
        #[arg(long, default_value = "none", value_enum)]
        priority: JobPriority,
    },
    /// Remove all temporary files and directories
    Clean {
        /// Path to the media directory
        path: PathBuf,
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
    },
    /// Validate Plex naming scheme conformity
    Validate {
        /// Path to the media directory to validate
        path: PathBuf,
        /// Actually fix/rename files instead of just validating
        #[arg(long)]
        fix: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "plexify=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Add {
            file,
            work_dir,
            preset,
        } => {
            let work_root = work_dir.unwrap_or_else(|| std::env::current_dir().unwrap());
            info!(
                "Starting add command for file: {:?}, work: {:?}, preset: {:?}",
                file, work_root, preset
            );
            AddCommand::new(file, work_root, preset).execute().await
        }
        Commands::Scan {
            path,
            work_dir,
            preset,
        } => {
            let work_root = work_dir.unwrap_or_else(|| std::env::current_dir().unwrap());
            info!(
                "Starting scan command for path: {:?}, work: {:?}, preset: {:?}",
                path, work_root, preset
            );
            ScanCommand::new(path, work_root, preset).execute().await
        }
        Commands::Work {
            path,
            work_dir,
            background,
            priority,
        } => {
            let work_root = work_dir.unwrap_or_else(|| std::env::current_dir().unwrap());
            info!(
                "Starting work command for path: {:?}, work: {:?}, background: {}, priority: {:?}",
                path, work_root, background, priority
            );
            WorkCommand::new(path, work_root, background, priority)
                .execute()
                .await
        }
        Commands::Clean { path, work_dir } => {
            let work_root = work_dir.unwrap_or_else(|| std::env::current_dir().unwrap());
            info!(
                "Starting clean command for path: {:?}, work: {:?}",
                path, work_root
            );
            CleanCommand::new(path, work_root).execute().await
        }
        Commands::Validate { path, fix } => {
            if fix {
                info!("Starting validate command with --fix for path: {:?}", path);
            } else {
                info!("Starting validate command for path: {:?}", path);
            }
            let validate_cmd = ValidateCommand::new(path, fix);
            match validate_cmd.execute().await {
                Ok(report) => {
                    validate_cmd.print_report(&report);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };

    if let Err(e) = result {
        error!("Command failed: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
