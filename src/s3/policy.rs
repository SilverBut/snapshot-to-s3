//! Timeouts and retry limits of the S3 transport.

use anyhow::{bail, Context, Result};
use std::env;
use std::time::Duration;

const DAY: Duration = Duration::from_secs(86400);

/// Transfer limits are based on recent progress, never total transfer duration.
#[derive(Clone, Debug)]
pub struct HttpPolicy {
    /// Rolling window for the minimum-throughput check of transfers.
    pub throughput_window: Duration,
    /// Bytes that must move within each window.
    pub minimum_bytes_per_window: u64,
    /// Whole-request timeout of HEAD, DELETE and LIST.
    pub control_timeout: Duration,
    /// GET retries and resumes per download.
    pub get_retries: usize,
    /// First GET retry delay; doubles per retry, capped at 60 s.
    pub retry_backoff: Duration,
}

impl Default for HttpPolicy {
    fn default() -> Self {
        Self {
            throughput_window: Duration::from_secs(30),
            minimum_bytes_per_window: 1024,
            control_timeout: Duration::from_secs(120),
            get_retries: 3,
            retry_backoff: Duration::from_millis(200),
        }
    }
}

impl HttpPolicy {
    /// Defaults overridden by the `SNAPSHOT_TO_S3_HTTP_*` environment variables.
    pub fn from_env() -> Result<Self> {
        let default = Self::default();
        Ok(Self {
            throughput_window: Duration::from_secs(env_u64(
                "SNAPSHOT_TO_S3_HTTP_WINDOW_SECS",
                default.throughput_window.as_secs(),
            )?),
            minimum_bytes_per_window: env_u64(
                "SNAPSHOT_TO_S3_HTTP_MIN_BYTES",
                default.minimum_bytes_per_window,
            )?,
            control_timeout: Duration::from_secs(env_u64(
                "SNAPSHOT_TO_S3_HTTP_CONTROL_TIMEOUT_SECS",
                default.control_timeout.as_secs(),
            )?),
            get_retries: env_u64(
                "SNAPSHOT_TO_S3_HTTP_GET_RETRIES",
                default.get_retries as u64,
            )?
            .try_into()
            .context("HTTP GET retry count exceeds platform limit")?,
            retry_backoff: Duration::from_millis(env_u64(
                "SNAPSHOT_TO_S3_HTTP_BACKOFF_MILLIS",
                default.retry_backoff.as_millis() as u64,
            )?),
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.throughput_window.is_zero()
            || self.throughput_window > DAY
            || self.minimum_bytes_per_window == 0
            || self.control_timeout.is_zero()
            || self.control_timeout > DAY
            || self.get_retries > 100
            || self.retry_backoff > Duration::from_secs(60)
        {
            bail!("invalid HTTP transfer policy");
        }
        Ok(())
    }

    /// Delay before GET retry number `retry` (zero-based).
    pub(super) fn retry_delay(&self, retry: usize) -> Duration {
        self.retry_backoff
            .saturating_mul(1 << retry.min(8))
            .min(Duration::from_secs(60))
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} must be an unsigned integer")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("read {name}")),
    }
}
