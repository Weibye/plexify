use anyhow::{bail, Context, Result};
use std::env::{self, VarError};

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
    /// Load configuration from environment variables, with defaults for those not set.
    ///
    /// A variable that is set but cannot be read is an error, not the default: a typo
    /// should not quietly change how often a worker looks for work.
    pub fn from_env() -> Result<Self> {
        let sleep_interval = match env::var("SLEEP_INTERVAL") {
            Err(VarError::NotPresent) => None,
            Err(VarError::NotUnicode(value)) => {
                bail!("SLEEP_INTERVAL={value:?} is not valid Unicode")
            }
            Ok(value) => Some(value),
        };
        Self::with_sleep_interval(sleep_interval.as_deref())
    }

    fn with_sleep_interval(value: Option<&str>) -> Result<Self> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        let sleep_interval = value.parse().with_context(|| {
            format!("SLEEP_INTERVAL={value:?} is not a whole number of seconds")
        })?;
        Ok(Self { sleep_interval })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { sleep_interval: 60 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_sleep_interval_is_the_default() {
        assert_eq!(
            Config::with_sleep_interval(None).unwrap().sleep_interval,
            60
        );
    }

    #[test]
    fn a_sleep_interval_is_read_in_seconds() {
        assert_eq!(
            Config::with_sleep_interval(Some("5"))
                .unwrap()
                .sleep_interval,
            5
        );
    }

    #[test]
    fn a_sleep_interval_that_is_not_a_number_is_an_error() {
        for value in ["", "5s", "-1", "1.5"] {
            let error = Config::with_sleep_interval(Some(value)).unwrap_err();
            assert!(
                format!("{error:#}").contains("SLEEP_INTERVAL"),
                "{value:?}: {error:#}"
            );
        }
    }
}
