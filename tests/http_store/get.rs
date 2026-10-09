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
    assert!(
        !request
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("content-length:")),
        "GET requests must not carry an empty upload body"
    );
    Ok(())
}

async fn bounded_fixture(
    responses: Vec<String>,
) -> Result<(String, tokio::task::JoinHandle<Vec<u8>>)> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let Ok(Ok((stream, _))) =
                tokio::time::timeout(Duration::from_millis(100), listener.accept()).await
            else {
                break;
            };
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                captured.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            let _ = reader.get_mut().write_all(response.as_bytes()).await;
        }
        captured
    });
    Ok((format!("http://{address}"), task))
}

#[tokio::test]
async fn get_retry_budget_is_exact_and_retry_counter_advances() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = bounded_fixture(vec![
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        get_reply(None, 2, "\"eventual\"", "ok"),
    ])
    .await?;
    let mut policy = fast_policy();
    policy.get_retries = 2;
    policy.retry_backoff = Duration::from_millis(1);
    let store = HttpStore::new_with_policy(config(endpoint), policy).await?;
    let result = tokio::time::timeout(Duration::from_secs(1), store.get("retry", None, None)).await;
    assert!(
        result.is_ok(),
        "GET retry loop failed to advance its budget"
    );
    assert!(
        result.unwrap().is_err(),
        "GET must stop after exactly two retries"
    );
    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 3);
    Ok(())
}

#[tokio::test]
async fn resume_retries_transient_responses_only_within_exact_budget() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = bounded_fixture(vec![
        get_reply(None, 6, "\"pinned\"", "abc"),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        get_reply(Some("bytes 3-5/6"), 3, "\"pinned\"", "def"),
    ])
    .await?;
    let mut policy = fast_policy();
    policy.get_retries = 2;
    policy.retry_backoff = Duration::from_millis(1);
    let store = HttpStore::new_with_policy(config(endpoint), policy).await?;
    let mut reader = store.get("resume", None, None).await?;
    let mut bytes = Vec::new();
    let error = reader.read_to_end(&mut bytes).await.unwrap_err();
    assert_eq!(bytes, b"abc");
    assert!(error.to_string().contains("GET resume failed"));
    assert_eq!(split_captured_requests(&server.await?).len(), 3);
    Ok(())
}

#[tokio::test]
async fn etag_validation_checks_both_quotes_and_accepts_minimum_length() -> Result<()> {
    let _env = EnvGuard::new();
    for (etag, valid) in [("plain\"", false), ("\"plain", false), ("\"\"", true)] {
        let response = get_reply(None, 1, etag, "x");
        let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
        let store = HttpStore::new(config(endpoint)).await?;
        let result = store.get("etag", None, None).await;
        if valid {
            let mut reader = result?;
            let mut body = Vec::new();
            reader.read_to_end(&mut body).await?;
            assert_eq!(body, b"x");
        } else {
            let error = result.err().expect("malformed ETag must be rejected");
            assert!(error.to_string().contains("GET requires a strong ETag"));
        }
        let _ = server.await?;
    }
    Ok(())
}

#[tokio::test]
async fn zero_start_range_is_not_divided_and_extreme_range_uses_subtraction() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture(Box::leak(
        get_reply(Some("bytes 0-2/3"), 3, "\"range\"", "abc").into_boxed_str(),
    ))
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let mut reader = store.get("range", None, Some((0, 2))).await?;
    let mut body = Vec::new();
    reader.read_to_end(&mut body).await?;
    assert_eq!(body, b"abc");
    assert!(String::from_utf8(server.await?)?.contains("range: bytes=0-2"));

    let end = u64::MAX;
    let range_response = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 1-{end}/{end}\r\nETag: \"huge\"\r\nConnection: close\r\n\r\nx"
    );
    let (endpoint, server) = fixture(Box::leak(range_response.into_boxed_str())).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = store
        .get("range", None, Some((1, u64::MAX)))
        .await
        .err()
        .expect("the advertised range must not fit the response");
    assert!(error
        .to_string()
        .contains("range GET Content-Range/Content-Length mismatch"));
    assert!(String::from_utf8(server.await?)?.contains("range: bytes=1-18446744073709551615"));
    Ok(())
}

#[tokio::test]
async fn range_bounds_accept_single_byte_and_reject_reversed_pair() -> Result<()> {
    let _env = EnvGuard::new();
    let response = get_reply(Some("bytes 1-1/2"), 1, "\"single\"", "x");
    let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let mut reader = store.get("range", None, Some((1, 1))).await?;
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    assert_eq!(bytes, b"x");
    assert!(String::from_utf8(server.await?)?.contains("range: bytes=1-1"));

    let store = HttpStore::new(config("http://127.0.0.1:1".into())).await?;
    let error = store
        .get("range", None, Some((2, 1)))
        .await
        .err()
        .expect("a reversed range must fail before sending a request");
    assert_eq!(error.to_string(), "invalid byte range: start exceeds end");
    Ok(())
}
