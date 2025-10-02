pub mod commands;
pub mod config;
pub mod ffmpeg;
pub mod ignore;
pub mod job;
pub mod queue;
pub mod worker;

use clap::ValueEnum;

/// Job prioritization methods for the work command
#[derive(Debug, Clone, ValueEnum, PartialEq)]
pub enum JobPriority {
    /// No prioritization - process jobs in order found (default)
    None,
    /// Prioritize episodes within series, older created jobs first
    Episode,
}
