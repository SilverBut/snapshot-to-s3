use super::support::{
    config, fast_policy, get_reply, paced_fixture, read_completion_request,
    split_captured_requests, EnvGuard,
};
use anyhow::Result;
use bytes::Bytes;
use snapshot_to_s3::{
    model::MetadataMap,
    s3::{is_definite_rejection, is_retryable, HttpStore},
    store::{ObjectStore, Part},
};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[tokio::test]
async fn healthy_get_outlives_control_timeout_and_many_throughput_windows() -> Result<()> {
    let _env = EnvGuard::new();
    let mut pieces = vec![(Duration::ZERO, get_reply(None, 128, "\"pinned\"", ""))];
    pieces.extend((0..16).map(|_| (Duration::from_millis(25), "abcdefgh".into())));
    let (endpoint, server) = paced_fixture(vec![pieces]).await?;
    let store = HttpStore::new_with_policy(config(endpoint), fast_policy()).await?;
    let mut reader = store.get("stream", None, None).await?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"abcdefgh".repeat(16));
    assert_eq!(split_captured_requests(&server.await?).len(), 1);
    Ok(())
}

#[tokio::test]
async fn get_throughput_detects_dribble_not_only_idle() -> Result<()> {
    let _env = EnvGuard::new();
    let mut slow = vec![(Duration::ZERO, get_reply(None, 16, "\"pinned\"", ""))];
    slow.extend((0..8).map(|_| (Duration::from_millis(30), "a".into())));
    let (endpoint, server) = paced_fixture(vec![slow]).await?;
    let mut policy = fast_policy();
    policy.get_retries = 0;
    let store = HttpStore::new_with_policy(config(endpoint), policy).await?;
    let mut reader = store.get("stream", None, None).await?;
    let mut bytes = Vec::new();
    let error = reader.read_to_end(&mut bytes).await.unwrap_err();
    assert!(error.to_string().contains("throughput below"));
    assert!(bytes.len() < 8);
    assert_eq!(split_captured_requests(&server.await?).len(), 1);
    Ok(())
}

#[tokio::test]
async fn get_stall_resumes_from_consumed_offset() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = paced_fixture(vec![
        vec![
            (Duration::ZERO, get_reply(None, 8, "\"pinned\"", "abc")),
            (Duration::from_millis(250), "unused".into()),
        ],
        vec![(
            Duration::ZERO,
            get_reply(Some("bytes 3-7/8"), 5, "\"pinned\"", "defgh"),
        )],
    ])
    .await?;
    let store = HttpStore::new_with_policy(config(endpoint), fast_policy()).await?;
    let mut reader = store.get("stream", None, None).await?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"abcdefgh");
    assert_eq!(split_captured_requests(&server.await?).len(), 2);
    Ok(())
}

#[tokio::test]
#[ignore = "takes 125 seconds to regress the former 120-second whole-request timeout"]
async fn healthy_get_exceeds_former_120_second_transfer_cap() -> Result<()> {
    let _env = EnvGuard::new();
    let chunk = "x".repeat(2048);
    let mut pieces = vec![(
        Duration::ZERO,
        get_reply(None, 125 * chunk.len(), "\"pinned\"", ""),
    )];
    pieces.extend((0..125).map(|_| (Duration::from_secs(1), chunk.clone())));
    let (endpoint, server) = paced_fixture(vec![pieces]).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let started = tokio::time::Instant::now();
    let mut reader = store.get("long-stream", None, None).await?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    assert!(started.elapsed() > Duration::from_secs(120));
    assert_eq!(bytes.len(), 125 * 2048);
    assert_eq!(split_captured_requests(&server.await?).len(), 1);
    Ok(())
}

#[tokio::test]
async fn blocked_put_body_fails_throughput_without_retry_or_definite_rejection() -> Result<()> {
    let _env = EnvGuard::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::with_capacity(1, stream);
        let mut request = String::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        // Deliberately never consume the body: transport backpressure must trip the guard.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "mutating PUT must not be retried internally"
        );
        request
    });
    let store =
        HttpStore::new_with_policy(config(format!("http://{address}")), fast_policy()).await?;
    let error = store
        .put(
            "stream",
            Bytes::from(vec![0u8; 16 * 1024 * 1024]),
            &MetadataMap::new(),
        )
        .await
        .expect_err("blocked transfer must fail throughput policy");
    assert!(format!("{error:#}").contains("throughput below"));
    assert!(
        is_retryable(&error),
        "retained multipart bytes may be retried by the caller"
    );
    assert!(!is_definite_rejection(&error));
    assert!(server.await?.starts_with("PUT "));
    Ok(())
}

#[tokio::test]
async fn stalled_post_completion_remains_unknown_and_is_not_retried() -> Result<()> {
    let _env = EnvGuard::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut request = String::new();
        read_completion_request(&mut reader, Some(&mut request)).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err(),
            "completion POST must not be retried internally"
        );
        request
    });
    let store =
        HttpStore::new_with_policy(config(format!("http://{address}")), fast_policy()).await?;
    let error = store
        .complete_upload(
            "stream",
            "upload",
            &[Part {
                number: 1,
                etag: "\"part\"".into(),
            }],
        )
        .await
        .expect_err("stalled completion must fail");
    assert!(format!("{error:#}").contains("throughput below"));
    assert!(!is_definite_rejection(&error));
    assert!(server.await?.starts_with("POST "));
    Ok(())
}

#[tokio::test]
async fn post_completion_response_body_stall_and_dribble_remain_unknown() -> Result<()> {
    let _env = EnvGuard::new();
    for dribble in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            read_completion_request(&mut reader, None).await;
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n<")
                .await
                .unwrap();
            if dribble {
                for _ in 0..8 {
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    if reader.get_mut().write_all(b"a").await.is_err() {
                        break;
                    }
                }
            } else {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(30), listener.accept())
                    .await
                    .is_err(),
                "uncertain completion response must not be retried"
            );
        });
        let store =
            HttpStore::new_with_policy(config(format!("http://{address}")), fast_policy()).await?;
        let error = store
            .complete_upload(
                "stream",
                "upload",
                &[Part {
                    number: 1,
                    etag: "\"part\"".into(),
                }],
            )
            .await
            .expect_err("completion response must be throughput guarded");
        assert!(format!("{error:#}").contains("throughput below"));
        assert!(is_retryable(&error));
        assert!(!is_definite_rejection(&error));
        server.await?;
    }
    Ok(())
}
