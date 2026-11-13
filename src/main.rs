use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use plexify::commands::{
    add::AddCommand, audit::AuditCommand, clean::CleanCommand, job_processor::JobProcessorConfig,
    scan::ScanCommand, status::StatusCommand, undo::UndoCommand, validate::ValidateCommand,
    work::WorkCommand,
};
use plexify::job::QualityPreset;
use plexify::queue::QueueDirectory;
use plexify::JobPriority;

#[derive(Parser)]
#[command(
    name = "plexify",
    about = "Names a Plex library's files the way Plex reads them, and converts what its clients cannot Direct Play",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

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
        #[arg(
            long,
            short = 'p',
            value_enum,
            ignore_case = true,
            hide_possible_values = true
        )]
        preset: Option<QualityPreset>,
    },
    /// Scan a directory for media files and create transcoding jobs
    Scan {
        /// Path to the media directory to scan
        path: PathBuf,
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
        /// Quality preset for encoding. Available: fast, balanced, quality, ultrafast, archive
        #[arg(
            long,
            short = 'p',
            value_enum,
            ignore_case = true,
            hide_possible_values = true
        )]
        preset: Option<QualityPreset>,
        /// A client every queued file has to play on: a built-in envelope's
        /// name, or the path to a TOML one. Repeat it to name more than one,
        /// and each file is queued with the most expensive work any of them
        /// needs - that is the only answer that satisfies all of them. Naming
        /// none probes nothing and queues by extension, as before.
        #[arg(long, short = 't')]
        target: Vec<String>,
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
    /// Report what is in the queue, and why anything in it is not moving
    Status {
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
    },
    /// Delete a work root's queue directories, after reporting what is in them
    Clean {
        /// Path to the work directory (defaults to current working directory)
        #[arg(long, short = 'w')]
        work_dir: Option<PathBuf>,
        /// Empty only these queue directories, instead of all four.
        /// Comma-separated: queue, in-progress, completed, failed
        #[arg(long, value_delimiter = ',', value_enum)]
        only: Vec<QueueDirectory>,
        /// Report what would be deleted and delete nothing
        #[arg(long)]
        dry_run: bool,
        /// Answer the confirmation in advance, for scripted use
        #[arg(long, short = 'y')]
        yes: bool,
        /// Also delete a claim whose worker is still checking in. `--yes` does
        /// not cover this: it is the one case where cleaning corrupts a run
        /// that is still going.
        #[arg(long)]
        force: bool,
    },
    /// Put a library back the way a fix run found it
    Undo {
        /// The plan file a `validate --fix` run wrote
        plan: PathBuf,
        /// Carry out the reversal instead of only reporting it
        #[arg(long)]
        apply: bool,
    },
    /// Report what each file needs before a client will Direct Play it
    Audit {
        /// Path to the media directory to audit
        path: PathBuf,
        /// The client to judge against: a built-in envelope's name, or the path
        /// to a TOML one. There is no default, because picking a device for the
        /// reader would be a claim about their living room.
        #[arg(long, short = 't')]
        target: String,
    },
    /// Report what in the library is not in canonical form
    Validate {
        /// Path to the media directory to validate
        path: PathBuf,
        /// Carry out the renames instead of only reporting them
        #[arg(long)]
        fix: bool,
        /// The directory holding Series/, Anime/ and Movies/, when it cannot be
        /// worked out from the path - a media root named after a library root
        /// and holding only one is the case that needs it
        #[arg(long)]
        library_root: Option<PathBuf>,
    },
}

/// Make a path the user typed absolute, without touching the filesystem.
///
/// A job's id is derived from its input path, so a root that stays relative
/// gives the same file a different id depending on where the command was run
/// from, and one file can then be queued, and transcoded, twice. Resolve only
/// what the user typed, never a name found by walking the library: on Windows
/// `absolute` strips trailing dots and spaces, so a walked name ending in one
/// would come out naming a different file.
fn resolve(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("Cannot resolve {:?}", path))
}

#[tokio::main]
async fn main() -> ExitCode {
    // Logs are diagnostics and go to stderr; stdout carries only what a command
    // deliberately prints, so a report can be piped or redirected on its own.
    // `fmt::layer()` defaults to stdout, so the writer has to be said out loud -
    // without it `RUST_LOG`, a documented variable, corrupts the output stream
    // it is turned up to investigate.
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "plexify=info".into()),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    match run(Cli::parse().command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` prints the whole context chain on one line, so code below
            // may say what it was doing with `.context(...)` and trust that the
            // cause still reaches the user. `{}` prints only the outermost layer.
            error!("Command failed: {:#}", e);
            ExitCode::FAILURE
        }
    }
}

async fn run(command: Commands) -> Result<()> {
    match command {
        Commands::Add {
            file,
            work_dir,
            preset,
        } => {
            let file = resolve(&file)?;
            let work_root = resolve(&work_dir.unwrap_or_else(|| ".".into()))?;
            info!(
                "Starting add command for file: {:?}, work: {:?}, preset: {:?}",
                file, work_root, preset
            );
            let config = JobProcessorConfig::from_preset(preset);
            AddCommand::new(file, work_root, config).execute().await
        }
        Commands::Scan {
            path,
            work_dir,
            preset,
            target,
        } => {
            let path = resolve(&path)?;
            let work_root = resolve(&work_dir.unwrap_or_else(|| ".".into()))?;
            info!(
                "Starting scan command for path: {:?}, work: {:?}, preset: {:?}, targets: {:?}",
                path, work_root, preset, target
            );
            let config = JobProcessorConfig::from_preset(preset);
            ScanCommand::new(path, work_root, config, &target)?
                .execute()
                .await
        }
        Commands::Work {
            path,
            work_dir,
            background,
            priority,
        } => {
            let path = resolve(&path)?;
            let work_root = resolve(&work_dir.unwrap_or_else(|| ".".into()))?;
            info!(
                "Starting work command for path: {:?}, work: {:?}, background: {}, priority: {:?}",
                path, work_root, background, priority
            );
            WorkCommand::new(path, work_root, background, priority)
                .execute()
                .await
        }
        Commands::Status { work_dir } => {
            let work_root = resolve(&work_dir.unwrap_or_else(|| ".".into()))?;
            info!("Starting status command for work: {:?}", work_root);

            // No media path is asked for. Nothing this command reports is
            // resolved against the media root, and requiring a directory that
            // does not affect the answer would be a wart on the one command
            // whose whole purpose is to be easy to reach for.
            let status = StatusCommand::new(work_root).execute().await?;
            print!("{}", plexify::commands::status::render(&status));
            Ok(())
        }
        Commands::Clean {
            work_dir,
            only,
            dry_run,
            yes,
            force,
        } => {
            let work_root = resolve(&work_dir.unwrap_or_else(|| ".".into()))?;
            info!("Starting clean command for work: {:?}", work_root);
            CleanCommand::new(work_root)
                .only(&only)
                .dry_run(dry_run)
                .assume_yes(yes)
                .force(force)
                .execute()
                .await
        }
        Commands::Undo { plan, apply } => {
            info!("Starting undo for plan: {:?}, apply: {}", plan, apply);
            UndoCommand::new(plan, apply).execute().await
        }
        Commands::Audit { path, target } => {
            info!("Starting audit for path: {:?}, target: {}", path, target);
            let audit = AuditCommand::new(path, &target)?;
            let report = audit.execute().await?;
            audit.print_report(&report);
            Ok(())
        }
        Commands::Validate {
            path,
            fix,
            library_root,
        } => {
            info!(
                "Starting validate command for path: {:?}, fix: {}, library root: {:?}",
                path, fix, library_root
            );
            let validate = match library_root {
                Some(root) => ValidateCommand::rooted_at(path, root)?,
                None => ValidateCommand::new(path),
            };
            let validate = if fix { validate.fixing() } else { validate };
            validate.execute().await
        }
    }
}
