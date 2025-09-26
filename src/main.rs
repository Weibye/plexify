use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::{info, error};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod config;
mod job;
mod queue;
mod worker;
mod ffmpeg;

use commands::{scan::ScanCommand, work::WorkCommand, clean::CleanCommand};

#[derive(Parser)]
#[command(
    name = "plexify",
    about = "A simple, distributed media transcoding CLI tool",
    long_about = "Converts .webm and .mkv files to .mp4 format with subtitle support, optimized for Plex media servers."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan a directory for media files and create transcoding jobs
    Scan {
        /// Path to the media directory to scan
        path: PathBuf,
    },
    /// Process jobs from the queue
    Work {
        /// Path to the media directory containing the job queue
        path: PathBuf,
        /// Run worker in background with low priority
        #[arg(long, short)]
        background: bool,
    },
    /// Remove all temporary files and directories
    Clean {
        /// Path to the media directory to clean
        path: PathBuf,
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
        Commands::Scan { path } => {
            info!("Starting scan command for path: {:?}", path);
            ScanCommand::new(path).execute().await
        }
        Commands::Work { path, background } => {
            info!("Starting work command for path: {:?}, background: {}", path, background);
            WorkCommand::new(path, background).execute().await
        }
        Commands::Clean { path } => {
            info!("Starting clean command for path: {:?}", path);
            CleanCommand::new(path).execute().await
        }
    };

    if let Err(e) = result {
        error!("Command failed: {}", e);
        std::process::exit(1);
    }

    Ok(())
}
