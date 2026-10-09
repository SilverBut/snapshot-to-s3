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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScopedEnv;

    const VARS: [&str; 5] = [
        "SNAPSHOT_TO_S3_HTTP_WINDOW_SECS",
        "SNAPSHOT_TO_S3_HTTP_MIN_BYTES",
        "SNAPSHOT_TO_S3_HTTP_CONTROL_TIMEOUT_SECS",
        "SNAPSHOT_TO_S3_HTTP_GET_RETRIES",
        "SNAPSHOT_TO_S3_HTTP_BACKOFF_MILLIS",
    ];

    #[test]
    fn environment_overrides_every_field_and_rejects_non_numbers() {
        let mut env = ScopedEnv::new(&VARS.map(|name| (name, None)));
        let defaults = HttpPolicy::from_env().unwrap();
        assert_eq!(defaults.throughput_window, Duration::from_secs(30));
        assert_eq!(defaults.minimum_bytes_per_window, 1024);
        assert_eq!(defaults.control_timeout, Duration::from_secs(120));
        assert_eq!(defaults.get_retries, 3);
        assert_eq!(defaults.retry_backoff, Duration::from_millis(200));

        for (name, value) in VARS.into_iter().zip(["7", "11", "13", "17", "19"]) {
            env.set(name, Some(value));
        }
        let policy = HttpPolicy::from_env().unwrap();
        assert_eq!(policy.throughput_window, Duration::from_secs(7));
        assert_eq!(policy.minimum_bytes_per_window, 11);
        assert_eq!(policy.control_timeout, Duration::from_secs(13));
        assert_eq!(policy.get_retries, 17);
        assert_eq!(policy.retry_backoff, Duration::from_millis(19));

        env.set("SNAPSHOT_TO_S3_HTTP_GET_RETRIES", Some("three"));
        let error = HttpPolicy::from_env().unwrap_err().to_string();
        assert_eq!(
            error,
            "SNAPSHOT_TO_S3_HTTP_GET_RETRIES must be an unsigned integer"
        );
    }

    #[test]
    fn validate_accepts_limits_and_rejects_each_violation() {
        let at_limits = HttpPolicy {
            throughput_window: DAY,
            minimum_bytes_per_window: 1,
            control_timeout: DAY,
            get_retries: 100,
            retry_backoff: Duration::from_secs(60),
        };
        at_limits.validate().unwrap();
        HttpPolicy::default().validate().unwrap();

        let invalid: [fn(&mut HttpPolicy); 7] = [
            |p| p.throughput_window = Duration::ZERO,
            |p| p.throughput_window = DAY + Duration::from_secs(1),
            |p| p.minimum_bytes_per_window = 0,
            |p| p.control_timeout = Duration::ZERO,
            |p| p.control_timeout = DAY + Duration::from_secs(1),
            |p| p.get_retries = 101,
            |p| p.retry_backoff = Duration::from_millis(60_001),
        ];
        for (index, change) in invalid.iter().enumerate() {
            let mut policy = HttpPolicy::default();
            change(&mut policy);
            assert!(policy.validate().is_err(), "violation {index} accepted");
        }
    }

    #[test]
    fn retry_delay_doubles_and_caps_at_one_minute() {
        let policy = HttpPolicy {
            retry_backoff: Duration::from_millis(100),
            ..HttpPolicy::default()
        };
        let delays: Vec<_> = (0..12).map(|retry| policy.retry_delay(retry)).collect();
        let millis: Vec<_> = delays.iter().map(Duration::as_millis).collect();
        assert_eq!(
            millis,
            [100, 200, 400, 800, 1600, 3200, 6400, 12800, 25600, 25600, 25600, 25600]
        );
        let slow = HttpPolicy {
            retry_backoff: Duration::from_secs(1),
            ..HttpPolicy::default()
        };
        assert_eq!(slow.retry_delay(6), Duration::from_secs(60));
    }
}
