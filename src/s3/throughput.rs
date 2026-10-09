//! Minimum-throughput enforcement for HTTP transfers.

use super::HttpPolicy;
use anyhow::Result;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Ring buffer slots: one window of 64 buckets plus the boundary bucket.
const SLOTS: usize = 65;

/// Only time spent polling the network counts: caller/decryptor backpressure is not a stall.
pub(super) struct ThroughputGuard {
    window: Duration,
    bucket_width: Duration,
    minimum: u64,
    elapsed: Duration,
    bytes: u64,
    samples: [(u128, u64); SLOTS],
}

impl ThroughputGuard {
    pub(super) fn new(policy: &HttpPolicy) -> Self {
        Self {
            window: policy.throughput_window,
            bucket_width: Duration::from_nanos(
                policy.throughput_window.as_nanos().div_ceil(SLOTS as u128 - 1) as u64
            ),
            minimum: policy.minimum_bytes_per_window,
            elapsed: Duration::ZERO,
            bytes: 0,
            samples: [(u128::MAX, 0); SLOTS],
        }
    }

    pub(super) fn add(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        let bucket = self.elapsed.as_nanos() / self.bucket_width.as_nanos();
        let sample = &mut self.samples[(bucket % SLOTS as u128) as usize];
        if sample.0 != bucket {
            *sample = (bucket, 0);
        }
        sample.1 = sample.1.saturating_add(bytes);
    }

    fn check(&mut self) -> Result<()> {
        // Include the boundary bucket: this upper bound cannot falsely reject healthy progress.
        let oldest =
            self.elapsed.saturating_sub(self.window).as_nanos() / self.bucket_width.as_nanos();
        let newest = self.elapsed.as_nanos() / self.bucket_width.as_nanos();
        let recent = self
            .samples
            .iter()
            .filter(|sample| sample.0 >= oldest && sample.0 <= newest)
            .fold(0u64, |sum, sample| sum.saturating_add(sample.1));
        if self.elapsed >= self.window && recent < self.minimum {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "HTTP throughput below {} bytes per {:?}",
                    self.minimum, self.window
                ),
            )
            .into());
        }
        Ok(())
    }

    /// Polls `future`, failing if throughput over the recent window drops
    /// below the minimum. `progress` counts request-body bytes sent.
    pub(super) async fn wait<F: Future>(
        &mut self,
        future: F,
        progress: Option<&AtomicU64>,
    ) -> Result<F::Output> {
        self.check()?;
        tokio::pin!(future);
        let mut last = Instant::now();
        let tick = (self.window / 4).max(Duration::from_nanos(1));
        loop {
            tokio::select! {
                output = &mut future => {
                    self.elapsed += last.elapsed();
                    if let Some(progress) = progress {
                        self.add(progress.load(Ordering::Relaxed).saturating_sub(self.bytes));
                    }
                    return Ok(output);
                }
                _ = tokio::time::sleep(tick) => {
                    self.elapsed += last.elapsed();
                    last = Instant::now();
                    if let Some(progress) = progress {
                        self.add(progress.load(Ordering::Relaxed).saturating_sub(self.bytes));
                    }
                    self.check()?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::s3::is_retryable;

    #[test]
    fn fixed_history_is_bounded_under_many_distinct_chunks() {
        let mut guard = ThroughputGuard::new(&HttpPolicy::default());
        for _ in 0..1_000_000 {
            guard.elapsed += Duration::from_micros(100);
            guard.add(1);
        }
        assert_eq!(guard.samples.len(), 65);
        assert!(
            guard.check().is_ok(),
            "healthy recent progress must remain visible"
        );
        guard.elapsed += guard.window + guard.bucket_width;
        assert!(
            guard.check().is_err(),
            "expired bursts must not remain credited"
        );
    }

    #[test]
    fn cutoff_bucket_preserves_healthy_progress_then_expires() {
        let mut guard = ThroughputGuard::new(&HttpPolicy {
            throughput_window: Duration::from_millis(640),
            minimum_bytes_per_window: 8,
            ..HttpPolicy::default()
        });
        guard.elapsed = Duration::from_millis(5);
        guard.add(8);
        guard.elapsed = Duration::from_millis(641);
        guard.add(1);
        assert!(
            guard.check().is_ok(),
            "all nine bytes are still inside the actual window"
        );
        guard.elapsed = Duration::from_millis(649);
        assert!(
            guard.check().is_ok(),
            "the boundary upper bound may briefly retain old bytes"
        );
        guard.elapsed = Duration::from_millis(650);
        assert!(
            guard.check().is_err(),
            "old progress expires within one boundary bucket"
        );
        guard.add(8);
        assert!(
            guard.check().is_ok(),
            "new bytes in the current bucket count"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_receive_and_upload_exceed_120_seconds() {
        let policy = HttpPolicy::default();
        let mut receive = ThroughputGuard::new(&policy);
        for _ in 0..150 {
            receive
                .wait(tokio::time::sleep(Duration::from_secs(1)), None)
                .await
                .unwrap();
            receive.add(2048);
        }
        assert!(receive.elapsed > Duration::from_secs(120));

        let progress = AtomicU64::new(0);
        let mut upload = ThroughputGuard::new(&policy);
        let producer = async {
            for _ in 0..150 {
                tokio::time::sleep(Duration::from_secs(1)).await;
                progress.fetch_add(2048, Ordering::Relaxed);
            }
        };
        upload.wait(producer, Some(&progress)).await.unwrap();
        assert!(upload.elapsed > Duration::from_secs(120));
    }

    #[tokio::test(start_paused = true)]
    async fn rolling_window_rejects_dribble_and_does_not_bank_an_early_burst() {
        let mut guard = ThroughputGuard::new(&HttpPolicy::default());
        guard.add(1024 * 1024);
        let error = loop {
            match guard
                .wait(tokio::time::sleep(Duration::from_secs(1)), None)
                .await
            {
                Ok(()) => guard.add(1),
                Err(error) => break error,
            }
        };
        assert!(guard.elapsed >= Duration::from_secs(30));
        assert!(guard.elapsed < Duration::from_secs(32));
        assert!(is_retryable(&error));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_receive_upload_and_consumer_pause() {
        let policy = HttpPolicy::default();
        let mut receive = ThroughputGuard::new(&policy);
        receive
            .wait(tokio::time::sleep(Duration::from_secs(1)), None)
            .await
            .unwrap();
        receive.add(2048);
        tokio::time::sleep(Duration::from_secs(1000)).await;
        receive
            .wait(tokio::time::sleep(Duration::from_secs(1)), None)
            .await
            .unwrap();
        receive.add(2048);
        assert_eq!(receive.elapsed, Duration::from_secs(2));
        assert!(receive
            .wait(std::future::pending::<()>(), None)
            .await
            .is_err());
        let progress = AtomicU64::new(0);
        assert!(ThroughputGuard::new(&policy)
            .wait(std::future::pending::<()>(), Some(&progress))
            .await
            .is_err());
    }
}

