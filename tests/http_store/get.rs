use super::support::{
    config, fast_policy, fixture, fixture_sequence, get_reply, split_captured_requests,
    verify_wire_signature, EnvGuard,
};
use anyhow::Result;
use snapshot_to_s3::{s3::HttpStore, store::ObjectStore};
use std::time::Duration;
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn get_disconnect_resumes_exact_ciphertext_offset_and_caller_range() -> Result<()> {
    let _env = EnvGuard::new();
    for range in [None, Some((4, 11))] {
        let (initial_range, resume_range, expected) = if range.is_some() {
            (Some("bytes 4-11/20"), "bytes 7-11/20", "bytes=7-11")
        } else {
            (None, "bytes 3-7/8", "bytes=3-7")
        };
        let (endpoint, server) = fixture_sequence(vec![
            get_reply(initial_range, 8, "\"pinned\"", "abc"),
            get_reply(Some(resume_range), 5, "\"pinned\"", "defgh"),
        ])
        .await?;
        let store = HttpStore::new_with_policy(config(endpoint), fast_policy()).await?;
        let mut reader = store.get("ciphertext", Some("\"pinned\""), range).await?;
        let mut first = [0; 1];
        reader.read_exact(&mut first).await?;
        // Buffered ciphertext is consumed before issuing the resumed request.
        tokio::time::sleep(Duration::from_millis(120)).await;
        let mut bytes = first.to_vec();
        reader.read_to_end(&mut bytes).await?;
        assert_eq!(bytes, b"abcdefgh");
        let raw = String::from_utf8(server.await?)?;
        let requests = split_captured_requests(raw.as_bytes());
        assert_eq!(requests.len(), 2);
        assert!(requests[1].0.contains(expected));
        assert!(requests[1].0.contains("if-match: \"pinned\""));
        assert!(requests[1].0.contains("accept-encoding: identity"));
        verify_wire_signature(&format!("{}\r\n\r\n", requests[1].0), "fixture-secret");
    }
    Ok(())
}

#[tokio::test]
async fn get_pins_discovered_etag_and_retries_transient_headers() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        String::new(),
        "HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        get_reply(None, 6, "\"discovered\"", "abc"),
        get_reply(Some("bytes 3-5/6"), 3, "\"discovered\"", "def"),
    ])
    .await?;
    let mut policy = fast_policy();
    policy.get_retries = 3;
    let store = HttpStore::new_with_policy(config(endpoint), policy).await?;
    let mut reader = store.get("stream", None, None).await?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"abcdef");
    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 4);
    assert!(requests[3].0.contains("if-match: \"discovered\""));
    Ok(())
}

#[tokio::test]
async fn get_resume_rejects_changed_identity_range_and_length() -> Result<()> {
    let _env = EnvGuard::new();
    let resumes = [
        get_reply(Some("bytes 3-5/6"), 3, "\"changed\"", "def"),
        get_reply(Some("bytes 2-4/6"), 3, "\"pinned\"", "def"),
        get_reply(Some("bytes 3-5/7"), 3, "\"pinned\"", "def"),
        get_reply(Some("bytes 3-5/6"), 2, "\"pinned\"", "de"),
        get_reply(None, 3, "\"pinned\"", "def"),
        get_reply(Some("bytes 3-5/6"), 3, "\"pinned\"", "def").replace(
            "Content-Length:",
            "Content-Encoding: gzip\r\nContent-Length:",
        ),
        get_reply(Some("bytes 3-5/6"), 3, "W/\"pinned\"", "def"),
        "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ];
    for resume in resumes {
        let (endpoint, server) =
            fixture_sequence(vec![get_reply(None, 6, "\"pinned\"", "abc"), resume]).await?;
        let store = HttpStore::new_with_policy(config(endpoint), fast_policy()).await?;
        let mut reader = store.get("stream", None, None).await?;
        let mut bytes = Vec::new();
        assert!(reader.read_to_end(&mut bytes).await.is_err());
        assert_eq!(bytes, b"abc", "never emit unvalidated resumed bytes");
        assert_eq!(split_captured_requests(&server.await?).len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn get_disconnect_retry_budget_is_not_reset_after_progress() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        get_reply(None, 6, "\"pinned\"", "a"),
        get_reply(Some("bytes 1-5/6"), 5, "\"pinned\"", "b"),
        get_reply(Some("bytes 2-5/6"), 4, "\"pinned\"", "c"),
    ])
    .await?;
    let store = HttpStore::new_with_policy(config(endpoint), fast_policy()).await?;
    let mut reader = store.get("stream", None, None).await?;
    let mut bytes = Vec::new();
    let error = reader.read_to_end(&mut bytes).await.unwrap_err();
    assert!(error.to_string().contains("retries exhausted"));
    assert_eq!(bytes, b"abc");
    assert_eq!(split_captured_requests(&server.await?).len(), 3);
    Ok(())
}

#[tokio::test]
async fn range_get_checks_headers_and_stream_length() -> Result<()> {
    let _env = EnvGuard::new();
    let response = concat!(
        "HTTP/1.1 206 Partial Content\r\n",
        "Content-Length: 3\r\n",
        "Content-Range: bytes 4-6/10\r\n",
        "ETag: \"version-1\"\r\n",
        "Connection: close\r\n\r\n",
        "456"
    );
    let (endpoint, server) = fixture(response).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let mut reader = store
        .get("range", Some("\"version-1\""), Some((4, 6)))
        .await?;
    let mut body = Vec::new();
    reader.read_to_end(&mut body).await?;
    assert_eq!(body, b"456");
    let request = String::from_utf8(server.await?).expect("fixture request is UTF-8");
    assert!(request.contains("range: bytes=4-6"));
    assert!(request.contains("if-match: \"version-1\""));
    Ok(())
}
