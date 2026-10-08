use anyhow::Result;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use snapshot_to_s3::{
    http_store::{is_definite_rejection, is_retryable, HttpConfig, HttpPolicy, HttpStore},
    model::MetadataMap,
    store::{ObjectStore, Part},
    transfer::{upload_parts, UploadLimits},
};
use std::{
    io,
    sync::{Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    task::JoinHandle,
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    access: Option<String>,
    secret: Option<String>,
    token: Option<String>,
    disabled: Option<String>,
}

impl EnvGuard {
    fn new() -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let access = std::env::var("AWS_ACCESS_KEY_ID").ok();
        let secret = std::env::var("AWS_SECRET_ACCESS_KEY").ok();
        let token = std::env::var("AWS_SESSION_TOKEN").ok();
        let disabled = std::env::var("AWS_EC2_METADATA_DISABLED").ok();
        std::env::set_var("AWS_ACCESS_KEY_ID", "fixture-access");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "fixture-secret");
        std::env::set_var("AWS_SESSION_TOKEN", "fixture-token");
        std::env::remove_var("AWS_EC2_METADATA_DISABLED");
        Self {
            _lock: lock,
            access,
            secret,
            token,
            disabled,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        restore_env("AWS_ACCESS_KEY_ID", self.access.take());
        restore_env("AWS_SECRET_ACCESS_KEY", self.secret.take());
        restore_env("AWS_SESSION_TOKEN", self.token.take());
        restore_env("AWS_EC2_METADATA_DISABLED", self.disabled.take());
    }
}

fn restore_env(name: &str, value: Option<String>) {
    if let Some(value) = value {
        std::env::set_var(name, value);
    } else {
        std::env::remove_var(name);
    }
}

fn config(endpoint: String) -> HttpConfig {
    HttpConfig {
        bucket: "fixture-bucket".into(),
        endpoint: Some(endpoint),
        region: "us-east-1".into(),
        metadata_prefix: "x-fixture-meta".into(),
        signing_service: "s3".into(),
        path_style: true,
    }
}

async fn fixture(response: &'static str) -> Result<(String, JoinHandle<Vec<u8>>)> {
    fixture_sequence(vec![response.to_owned()]).await
}

async fn fixture_sequence(responses: Vec<String>) -> Result<(String, JoinHandle<Vec<u8>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let mut captured = Vec::new();
        for response in responses {
            let (stream, _) = listener.accept().await.expect("accept fixture request");
            let mut reader = BufReader::new(stream);
            let mut request = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader
                    .read_line(&mut line)
                    .await
                    .expect("read request line");
                request.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().expect("parse content length");
                    }
                }
            }
            let mut body = vec![0; content_length];
            reader
                .read_exact(&mut body)
                .await
                .expect("read request body");
            request.extend_from_slice(&body);
            captured.extend_from_slice(&request);
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write fixture response");
        }
        captured
    });
    Ok((format!("http://{address}"), task))
}

fn fast_policy() -> HttpPolicy {
    HttpPolicy {
        throughput_window: Duration::from_millis(100),
        minimum_bytes_per_window: 8,
        control_timeout: Duration::from_millis(25),
        get_retries: 2,
        retry_backoff: Duration::from_millis(1),
    }
}

fn get_reply(range: Option<&str>, length: usize, etag: &str, body: &str) -> String {
    let status = if range.is_some() {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    let range = range
        .map(|range| format!("Content-Range: {range}\r\n"))
        .unwrap_or_default();
    format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\nETag: {etag}\r\n{range}Connection: close\r\n\r\n{body}")
}

async fn paced_fixture(
    responses: Vec<Vec<(Duration, String)>>,
) -> Result<(String, JoinHandle<Vec<u8>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        let mut writers = Vec::new();
        for pieces in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                captured.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            writers.push(tokio::spawn(async move {
                for (delay, piece) in pieces {
                    tokio::time::sleep(delay).await;
                    if reader.get_mut().write_all(piece.as_bytes()).await.is_err() {
                        break;
                    }
                }
            }));
        }
        for writer in writers {
            writer.await.unwrap();
        }
        captured
    });
    Ok((format!("http://{address}"), server))
}

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
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("content-length: ") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
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
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("content-length: ") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
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

fn verify_wire_signature(request: &str, secret: &str) {
    let (head, body) = request.split_once("\r\n\r\n").expect("HTTP headers");
    let mut lines = head.lines();
    let request_line = lines.next().expect("request line");
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap();
    let target = request_parts.next().unwrap();
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let authorization = headers.get("authorization").unwrap();
    let signed_headers = authorization
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let canonical_headers = signed_headers
        .split(';')
        .map(|name| format!("{name}:{}\n", headers.get(name).unwrap().trim()))
        .collect::<String>();
    let query = raw_query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (
                aws_encode(&percent_decode(key)),
                aws_encode(&percent_decode(value)),
            )
        })
        .collect::<Vec<_>>();
    let mut query = query;
    query.sort();
    let canonical_query = query
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let payload_hash = headers.get("x-amz-content-sha256").unwrap();
    assert_eq!(payload_hash, &hex::encode(Sha256::digest(body.as_bytes())));
    let canonical_request = format!(
        "{method}\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
    );
    let credential = authorization
        .split("Credential=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let scope = credential.split_once('/').unwrap().1;
    let mut scope_parts = scope.split('/');
    let date = scope_parts.next().unwrap();
    let region = scope_parts.next().unwrap();
    let service = scope_parts.next().unwrap();
    assert_eq!(scope_parts.next(), Some("aws4_request"));
    assert_eq!(&headers["x-amz-date"][..8], date);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        headers["x-amz-date"],
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let hmac = |key: &[u8], input: &[u8]| {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(input);
        mac.finalize().into_bytes().to_vec()
    };
    let mut date_key = b"AWS4".to_vec();
    date_key.extend_from_slice(secret.as_bytes());
    let date_key = hmac(&date_key, date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    let signing_key = hmac(&service_key, b"aws4_request");
    let expected_signature = hex::encode(hmac(&signing_key, string_to_sign.as_bytes()));
    let actual_signature = authorization.split("Signature=").nth(1).unwrap().trim();
    assert_eq!(actual_signature, expected_signature);
}

fn split_captured_requests(raw: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut requests = Vec::new();
    let mut offset = 0;
    while offset < raw.len() {
        let relative_end = raw[offset..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("request header terminator");
        let header_end = offset + relative_end;
        let head = &raw[offset..header_end];
        let content_length = std::str::from_utf8(head)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| {
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        let body_end = body_start + content_length;
        requests.push((
            String::from_utf8(head.to_vec()).expect("request headers are UTF-8"),
            raw[body_start..body_end].to_vec(),
        ));
        offset = body_end;
    }
    requests
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap();
            decoded.push(u8::from_str_radix(hex, 16).unwrap());
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap()
}

fn aws_encode(input: &str) -> String {
    input
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn signs_put_and_sends_custom_metadata_headers_once_encoded() -> Result<()> {
    let _env = EnvGuard::new();
    let response = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let (endpoint, server) = fixture(response).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let mut metadata = MetadataMap::new();
    metadata.insert("gpg-key-id".into(), "fingerprint & value".into());
    store
        .put(
            "folder/a b/雪.bin",
            Bytes::from_static(b"payload"),
            &metadata,
        )
        .await?;
    let request = String::from_utf8(server.await?).expect("fixture request is UTF-8");
    assert!(request.starts_with("PUT /fixture-bucket/folder/a%20b/%E9%9B%AA.bin HTTP/1.1\r\n"));
    let host = endpoint_host_from_request(&request);
    assert!(host.starts_with("127.0.0.1:"));
    assert!(host.rsplit_once(':').unwrap().1.parse::<u16>().is_ok());
    verify_wire_signature(&request, "fixture-secret");
    assert!(request
        .to_ascii_lowercase()
        .contains("x-fixture-meta-gpg-key-id: fingerprint & value"));
    assert!(request.contains("x-amz-security-token: fixture-token"));
    assert!(request.contains("authorization: AWS4-HMAC-SHA256 Credential=fixture-access/"));
    assert!(request.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date;x-amz-security-token;x-fixture-meta-gpg-key-id"));
    assert!(request.ends_with("payload"));
    Ok(())
}

fn endpoint_host_from_request(request: &str) -> String {
    request
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .expect("wire Host header")
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

#[tokio::test]
async fn paginates_list_and_preserves_xml_decoded_keys() -> Result<()> {
    let _env = EnvGuard::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        for page in 0..2 {
            let (stream, _) = listener.accept().await.expect("accept page");
            let mut reader = BufReader::new(stream);
            let mut request = Vec::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.expect("read request");
                request.extend_from_slice(line.as_bytes());
                if line == "\r\n" {
                    break;
                }
            }
            captured.extend_from_slice(&request);
            let body = if page == 0 {
                "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>next +/雪</NextContinuationToken><Contents><Key>a&amp;b</Key></Contents></ListBucketResult>"
            } else {
                "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>next</Key></Contents></ListBucketResult>"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write listing");
        }
        captured
    });
    let store = HttpStore::new(config(format!("http://{address}"))).await?;
    assert_eq!(store.list("prefix/雪 +").await?, vec!["a&b", "next"]);
    let request = String::from_utf8(server.await?).expect("captured requests are UTF-8");
    assert!(request.contains("prefix%2F%E9%9B%AA%20%2B"));
    assert!(request.contains("continuation-token=next%20%2B%2F%E9%9B%AA"));
    let first_request_end = request.find("\r\n\r\n").unwrap() + 4;
    verify_wire_signature(&request[..first_request_end], "fixture-secret");
    Ok(())
}

#[tokio::test]
async fn completion_detects_embedded_error_and_xml_escapes_etag() -> Result<()> {
    let _env = EnvGuard::new();
    let response = concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Length: 74\r\n",
        "Connection: close\r\n\r\n",
        "<Error><Code>InternalError</Code><Message>try &amp; fail</Message></Error>"
    );
    let (endpoint, server) = fixture(response).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let result = store
        .complete_upload(
            "stream",
            "upload +/雪",
            &[Part {
                number: 1,
                etag: "\"etag<&\"".into(),
            }],
        )
        .await;
    let error = result.expect_err("embedded Error response must fail");
    assert!(format!("{error:#}").contains("InternalError"));
    assert!(format!("{error:#}").contains("try & fail"));
    assert!(is_definite_rejection(&error));
    let request = String::from_utf8(server.await?).expect("fixture request is UTF-8");
    assert!(request.contains("uploadId=upload%20%2B%2F%E9%9B%AA"));
    assert!(request.contains("&quot;etag&lt;&amp;&quot;"));
    Ok(())
}

#[tokio::test]
async fn unparsed_successful_completion_is_not_a_definite_rejection() -> Result<()> {
    let _env = EnvGuard::new();
    let body = "<Unexpected/>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = store
        .complete_upload(
            "stream",
            "upload-id",
            &[Part {
                number: 1,
                etag: "\"etag\"".into(),
            }],
        )
        .await
        .expect_err("unexpected successful response must not count as a commit");
    assert!(!is_definite_rejection(&error));
    assert!(!is_retryable(&error));
    let _ = server.await?;
    Ok(())
}

#[tokio::test]
async fn retry_classifier_only_marks_transient_statuses() -> Result<()> {
    let _env = EnvGuard::new();
    for (status, should_retry, should_be_definite) in [
        ("408 Request Timeout", true, false),
        ("429 Too Many Requests", true, true),
        ("503 Service Unavailable", true, false),
        ("500 Internal Server Error", true, false),
        ("409 Conflict", false, true),
        ("403 Forbidden", false, true),
    ] {
        let response =
            format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
        let store = HttpStore::new(config(endpoint)).await?;
        let error = store
            .upload_part("stream", "id", 1, Bytes::from_static(b"same bytes"))
            .await
            .expect_err("fixture error status should fail");
        assert_eq!(is_retryable(&error), should_retry, "status {status}");
        assert_eq!(
            is_definite_rejection(&error),
            should_be_definite,
            "definite rejection status {status}"
        );
        let _ = server.await?;
    }
    assert!(!is_retryable(&anyhow::anyhow!(
        "permanent local validation error"
    )));
    assert!(!is_retryable(
        &io::Error::new(io::ErrorKind::InvalidInput, "invalid request").into()
    ));
    assert!(!is_definite_rejection(&anyhow::anyhow!(
        "unparsed successful response"
    )));
    Ok(())
}

#[tokio::test]
async fn upload_parts_retries_identical_bytes_and_returns_only_final_etag() -> Result<()> {
    let _env = EnvGuard::new();
    let responses = [
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nETag: \"final-etag\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (endpoint, server) = fixture_sequence(responses).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let ciphertext = b"tiny".to_vec();
    let mut reader: snapshot_to_s3::model::Reader =
        Box::new(std::io::Cursor::new(ciphertext.clone()));
    let result = upload_parts(
        &store,
        "stream",
        "upload +/雪",
        &mut reader,
        ciphertext.len() as u64,
        &UploadLimits {
            min_part_size: 1,
            max_part_size: ciphertext.len() as u64,
            max_parts: 1,
            max_object_size: ciphertext.len() as u64,
            buffer_limit: ciphertext.len() as u64,
        },
        &tokio_util::sync::CancellationToken::new(),
    )
    .await?;
    assert_eq!(result.bytes, ciphertext.len() as u64);
    assert_eq!(result.parts.len(), 1);
    assert_eq!(result.parts[0].number, 1);
    assert_eq!(result.parts[0].etag, "\"final-etag\"");

    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 3);
    for (head, body) in &requests {
        assert!(head.starts_with(
            "PUT /fixture-bucket/stream?partNumber=1&uploadId=upload%20%2B%2F%E9%9B%AA HTTP/1.1"
        ));
        assert_eq!(body, &ciphertext);
    }
    assert_eq!(requests[0].1, requests[1].1);
    assert_eq!(requests[1].1, requests[2].1);
    Ok(())
}

#[tokio::test]
async fn head_returns_none_only_for_not_found_and_preserves_service_errors() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) =
        fixture("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
    let store = HttpStore::new(config(endpoint)).await?;
    assert!(store.head("missing").await?.is_none());
    let _ = server.await?;

    let body = "<Error><Code>AccessDenied</Code><Message>policy says no</Message></Error>";
    let response = format!(
        "HTTP/1.1 403 Forbidden\r\nx-amz-error-code: AccessDenied\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = store
        .head("private")
        .await
        .expect_err("403 is not a missing key");
    assert!(
        format!("{error:#}").contains("AccessDenied"),
        "unexpected HEAD error: {error:#}"
    );
    assert!(!is_retryable(&error));
    let _ = server.await?;
    Ok(())
}

#[tokio::test]
async fn conditional_capability_probe_rejects_ignored_metadata_and_cleans_lock_key() -> Result<()> {
    let _env = EnvGuard::new();
    let responses = [
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"probe-etag\"\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (endpoint, server) = fixture_sequence(responses).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = store
        .validate_conditional_put("backups/snapshot")
        .await
        .expect_err("a provider ignoring metadata must fail its capability probe");
    assert!(format!("{error:#}").contains("metadata prefix"));
    let requests = String::from_utf8(server.await?).expect("fixture requests are UTF-8");
    assert!(
        requests.contains("/fixture-bucket/backups/snapshot/.__snapshot-to-s3-condition-check/")
    );
    assert!(requests.contains("/.lock HTTP/1.1"));
    assert!(requests.contains("x-fixture-meta-http-store-capability:"));
    assert!(requests
        .contains("DELETE /fixture-bucket/backups/snapshot/.__snapshot-to-s3-condition-check/"));
    Ok(())
}
