//! Memory buffer for rate limiting streams

use anyhow::Result;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::time::{Duration, Instant};

/// A buffered stream reader with rate limiting capability
pub struct MBuffer<R> {
    inner: R,
    buffer_size: usize,
    rate_limit: Option<u64>, // bytes per second
    last_read: Instant,
    bytes_read_in_window: u64,
    window_start: Instant,
}

impl<R: AsyncRead + Unpin> MBuffer<R> {
    /// Create a new MBuffer with specified buffer size
    pub fn new(inner: R, buffer_size: usize) -> Self {
        let now = Instant::now();
        Self {
            inner,
            buffer_size,
            rate_limit: None,
            last_read: now,
            bytes_read_in_window: 0,
            window_start: now,
        }
    }
    
    /// Set rate limit in bytes per second
    pub fn with_rate_limit(mut self, bytes_per_second: u64) -> Self {
        self.rate_limit = Some(bytes_per_second);
        self
    }
    
    /// Check if we should throttle based on rate limit
    fn should_throttle(&mut self, bytes_to_read: usize) -> Option<Duration> {
        let rate_limit = match self.rate_limit {
            Some(limit) => limit,
            None => return None,
        };
        
        let now = Instant::now();
        let elapsed = now.duration_since(self.window_start);
        
        // Reset window every second
        if elapsed >= Duration::from_secs(1) {
            self.window_start = now;
            self.bytes_read_in_window = 0;
        }
        
        // Check if adding these bytes would exceed the limit
        let projected_total = self.bytes_read_in_window + bytes_to_read as u64;
        if projected_total > rate_limit {
            // Calculate how long to wait
            let remaining = Duration::from_secs(1).saturating_sub(elapsed);
            if !remaining.is_zero() {
                return Some(remaining);
            }
        }
        
        None
    }
    
    /// Get the inner reader
    pub fn into_inner(self) -> R {
        self.inner
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for MBuffer<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // Check if we should throttle
        if let Some(delay) = self.should_throttle(buf.remaining()) {
            // Wake up after the delay
            let waker = cx.waker().clone();
            let delay_clone = delay;
            tokio::spawn(async move {
                tokio::time::sleep(delay_clone).await;
                waker.wake();
            });
            return Poll::Pending;
        }
        
        // Limit read size to buffer size
        let _max_read = std::cmp::min(buf.remaining(), self.buffer_size);
        let filled_before = buf.filled().len();
        
        // Read from inner
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        
        // Update rate limiting stats
        if let Poll::Ready(Ok(())) = result {
            let bytes_read = buf.filled().len() - filled_before;
            self.bytes_read_in_window += bytes_read as u64;
            self.last_read = Instant::now();
        }
        
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_mbuffer_basic() {
        let data = vec![1u8; 1024];
        let cursor = Cursor::new(data);
        let mut mbuffer = MBuffer::new(cursor, 512);
        
        let mut output = Vec::new();
        mbuffer.read_to_end(&mut output).await.unwrap();
        
        assert_eq!(output.len(), 1024);
    }
    
    #[tokio::test]
    async fn test_mbuffer_with_rate_limit() {
        let data = vec![1u8; 1024];
        let cursor = Cursor::new(data);
        let mut mbuffer = MBuffer::new(cursor, 512).with_rate_limit(2048); // 2KB/s
        
        let start = Instant::now();
        let mut output = Vec::new();
        mbuffer.read_to_end(&mut output).await.unwrap();
        let elapsed = start.elapsed();
        
        assert_eq!(output.len(), 1024);
        // Should take less than 1 second for 1KB at 2KB/s
        assert!(elapsed < Duration::from_secs(1));
    }
}
