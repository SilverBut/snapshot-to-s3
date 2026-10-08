use snapshot_to_s3::rate;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncWrite, AsyncWriteExt};

#[derive(Default)]
struct VecWriter {
    bytes: Vec<u8>,
}

impl AsyncWrite for VecWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.bytes.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn rejects_zero_limit() {
    let (mut tx, mut rx) = tokio::io::duplex(8);
    tx.write_all(b"a").await.unwrap();
    tx.shutdown().await.unwrap();

    let mut out = VecWriter::default();
    let err = rate::copy_limited(&mut rx, &mut out, Some(0))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("greater than zero"));
}

#[tokio::test]
async fn rate_limit_waits_for_elapsed_schedule() {
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tokio::spawn(async move {
        tx.write_all(b"abcd").await.unwrap();
        tx.shutdown().await.unwrap();
    });

    let task = tokio::spawn(async move {
        let mut out = VecWriter::default();
        let copied = rate::copy_limited(&mut rx, &mut out, Some(2))
            .await
            .unwrap();
        (copied, out.bytes)
    });

    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!task.is_finished());

    let (copied, bytes) = tokio::time::timeout(std::time::Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(copied, 4);
    assert_eq!(bytes, b"abcd");
}
