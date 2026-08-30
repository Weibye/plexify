pub mod commands;
pub mod config;
pub mod ffmpeg;
pub mod fix;
pub mod ignore;
pub mod job;
pub mod naming;
pub mod paths;
pub mod probe;
pub mod queue;
pub mod subtitles;
pub mod target;
pub mod undo;

use clap::ValueEnum;

/// Job prioritization methods for the work command
#[derive(Debug, Clone, ValueEnum, PartialEq)]
pub enum JobPriority {
    /// No prioritization - process jobs in order found (default)
    None,
    /// Prioritize episodes within series, older created jobs first
    Episode,
}
