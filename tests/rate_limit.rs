use snapshot_to_s3::rate;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn rejects_zero_limit() {
    let (mut tx, mut rx) = tokio::io::duplex(8);
    tx.write_all(b"a").await.unwrap();
    tx.shutdown().await.unwrap();

    let mut out = Vec::new();
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
        let mut out = Vec::new();
        let copied = rate::copy_limited(&mut rx, &mut out, Some(2))
            .await
            .unwrap();
        (copied, out)
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
