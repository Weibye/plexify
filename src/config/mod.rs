use std::env;

/// Settings for the worker loop itself.
///
/// Encoding settings are not here: a job carries the `QualitySettings` it was
/// created with, so what a worker finds in its own environment must not change
/// how an existing job is encoded.
#[derive(Debug, Clone)]
pub struct Config {
    /// Seconds to wait before checking an empty queue again.
    pub sleep_interval: u64,
}

impl Config {
    /// Load configuration from environment variables with defaults
    pub fn from_env() -> Self {
        Self {
            sleep_interval: env::var("SLEEP_INTERVAL")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { sleep_interval: 60 }
    }
}
