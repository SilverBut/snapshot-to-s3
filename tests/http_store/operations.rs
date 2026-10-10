use super::support::{
    config, fast_policy, fixture, fixture_sequence, split_captured_requests, verify_wire_signature,
    EnvGuard,
};
use anyhow::Result;
use bytes::Bytes;
use snapshot_to_s3::{
    model::MetadataMap,
    s3::{is_retryable, HttpStore, LockDetectionMode},
    store::{probe_metadata, HeldLock, ObjectStore},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

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
async fn metadata_probe_rejects_wrong_size_and_cleans_fixed_sibling_key() -> Result<()> {
    let _env = EnvGuard::new();
    let responses = [
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"probe-etag\"\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (endpoint, server) = fixture_sequence(responses).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = probe_metadata(
        &store,
        "backups/snapshot/",
        "backups/snapshot/stream.encrypted",
        false,
    )
    .await
    .expect_err("a provider ignoring metadata must fail its capability probe");
    assert!(format!("{error:#}").contains("unexpected length"));
    let requests = String::from_utf8(server.await?).expect("fixture requests are UTF-8");
    assert!(requests.contains("/fixture-bucket/backups/snapshot/.metadata-probe HTTP/1.1"));
    assert!(requests.contains("x-fixture-meta-current-snapshot-id:"));
    assert!(requests.contains("DELETE /fixture-bucket/backups/snapshot/.metadata-probe"));
    for (head, body) in split_captured_requests(requests.as_bytes())
        .into_iter()
        .take(1)
    {
        assert!(head.starts_with("PUT "));
        assert!(head.lines().any(|line| line == "content-length: 31"));
        assert!(!head.to_ascii_lowercase().contains("transfer-encoding:"));
        assert_eq!(body, b"snapshot-to-s3 capability probe");
        assert!(!head.contains("if-none-match:"));
        verify_wire_signature(
            &format!("{head}\r\n\r\n{}", String::from_utf8_lossy(&body)),
            "fixture-secret",
        );
    }
    Ok(())
}

#[tokio::test]
async fn empty_put_and_multipart_post_send_explicit_zero_content_length() -> Result<()> {
    let _env = EnvGuard::new();
    let xml = "<InitiateMultipartUploadResult><UploadId>upload-id</UploadId></InitiateMultipartUploadResult>";
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{xml}",
            xml.len()
        ),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    store
        .put("empty", Bytes::new(), &MetadataMap::new())
        .await?;
    assert_eq!(
        store.create_upload("stream", &MetadataMap::new()).await?,
        "upload-id"
    );
    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 2);
    assert!(requests[0].0.starts_with("PUT /fixture-bucket/empty "));
    assert!(requests[1]
        .0
        .starts_with("POST /fixture-bucket/stream?uploads= "));
    for (head, body) in requests {
        assert!(head.lines().any(|line| line == "content-length: 0"));
        assert!(!head.to_ascii_lowercase().contains("transfer-encoding:"));
        assert!(body.is_empty());
        verify_wire_signature(&format!("{head}\r\n\r\n"), "fixture-secret");
    }
    Ok(())
}

#[tokio::test]
async fn configuration_rejects_each_invalid_required_field() -> Result<()> {
    let _env = EnvGuard::new();
    let mut invalid = Vec::new();
    for bucket in [
        "",
        "bad/bucket",
        "bad@bucket",
        "bad?bucket",
        "bad#bucket",
        "bad\0bucket",
    ] {
        let mut value = config("http://127.0.0.1:1".into());
        value.bucket = bucket.into();
        invalid.push(value);
    }
    let mut region = config("http://127.0.0.1:1".into());
    region.region = "  \t".into();
    invalid.push(region);
    let mut service = config("http://127.0.0.1:1".into());
    service.signing_service = "  ".into();
    invalid.push(service);
    for value in invalid {
        let error = match HttpStore::new_with_policy(value, fast_policy()).await {
            Ok(_) => panic!("invalid HTTP object-store configuration accepted"),
            Err(error) => error.to_string(),
        };
        assert_eq!(error, "invalid HTTP object-store configuration");
    }
    Ok(())
}

#[tokio::test]
async fn put_and_head_round_trip_metadata_and_reject_invalid_keys() -> Result<()> {
    let _env = EnvGuard::new();
    let metadata = MetadataMap::from([("owner_id".into(), "backup-team".into())]);
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nETag: \"stored\"\r\nx-fixture-meta-owner_id: backup-team\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    store
        .put("object", Bytes::from_static(b"data"), &metadata)
        .await?;
    let head = store.head("object").await?.unwrap();
    assert_eq!(head.size, 4);
    assert_eq!(head.etag, "\"stored\"");
    assert_eq!(head.metadata, metadata);
    let requests = String::from_utf8(server.await?)?;
    assert!(requests.contains("x-fixture-meta-owner_id: backup-team"));

    for key in ["", "bad key", "bad/key"] {
        let error = store
            .put(
                "object",
                Bytes::new(),
                &MetadataMap::from([(key.into(), "value".into())]),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid S3 metadata key"), "{error}");
    }
    Ok(())
}

#[tokio::test]
async fn list_response_body_limit_accepts_exact_limit_and_rejects_one_more_byte() -> Result<()> {
    let _env = EnvGuard::new();
    const LIMIT: usize = 1024 * 1024;
    for (size, should_succeed) in [(LIMIT, true), (LIMIT + 1, false)] {
        let open = "<ListBucketResult>";
        let close = "</ListBucketResult>";
        let body = format!(
            "{open}{}{close}",
            " ".repeat(size - open.len() - close.len())
        );
        assert_eq!(body.len(), size);
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let (endpoint, server) = fixture(Box::leak(response.into_boxed_str())).await?;
        let store = HttpStore::new(config(endpoint)).await?;
        let result = store.list("prefix").await;
        if should_succeed {
            assert_eq!(result.unwrap(), Vec::<String>::new());
        } else {
            let error = result.unwrap_err();
            assert!(format!("{error:#}").contains("exceeds 1048576-byte limit"));
        }
        let _ = server.await?;
    }
    Ok(())
}

#[tokio::test]
async fn head_and_delete_keep_the_whole_request_control_timeout() -> Result<()> {
    use std::time::Duration;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    let _env = EnvGuard::new();
    for method in ["HEAD", "DELETE"] {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let method_owned = method.to_owned();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            let response = if method_owned == "HEAD" {
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"empty\"\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = reader.get_mut().write_all(response.as_bytes()).await;
            request
        });
        let store =
            HttpStore::new_with_policy(config(format!("http://{address}")), fast_policy()).await?;
        let result = if method == "HEAD" {
            store.head("object").await.map(|_| ())
        } else {
            store.delete("object").await
        };
        assert!(
            result.is_err(),
            "{method} must obey the 25ms control timeout"
        );
        assert!(server.await?.starts_with(method));
    }
    Ok(())
}

#[tokio::test]
async fn metadata_probe_accepts_all_preserved_fields_and_provider_extras() -> Result<()> {
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    let _env = EnvGuard::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let mut captured = Vec::new();
        let mut metadata_headers = String::new();
        for index in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut lines = Vec::new();
            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap();
                    }
                    if name.starts_with("x-fixture-meta-") {
                        metadata_headers.push_str(&line);
                    }
                }
                lines.push(line);
                if lines.last().is_some_and(|line| line == "\r\n") {
                    break;
                }
            }
            captured.extend(lines.concat().as_bytes());
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).await.unwrap();
            captured.extend_from_slice(&body);
            let response = match index {
                0 => "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                2 => "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
                1 => format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: 31\r\nETag: \"probe\"\r\n{metadata_headers}x-fixture-meta-provider-extra: accepted\r\nConnection: close\r\n\r\n"
                ),
                _ => unreachable!(),
            };
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
        captured
    });
    let store = HttpStore::new(config(format!("http://{address}"))).await?;
    probe_metadata(
        &store,
        "backups/snapshot/",
        "backups/snapshot/stream.encrypted",
        false,
    )
    .await?;
    let requests = String::from_utf8(server.await?)?;
    assert_eq!(requests.matches("HTTP/1.1\r\n").count(), 3);
    for field in [
        "gpg-key-id",
        "fs-type",
        "vol-id",
        "current-snapshot-id",
        "base-snapshot-id",
        "base-object-key",
    ] {
        assert!(requests.contains(&format!("x-fixture-meta-{field}:")));
    }
    assert!(requests.contains("DELETE /fixture-bucket/backups/snapshot/.metadata-probe"));
    Ok(())
}

#[tokio::test]
async fn lock_acquisition_rejects_an_ignored_successful_conditional_put() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = HeldLock::acquire(&store, "backup/").await.err().unwrap();
    assert!(format!("{error:#}").contains("storage does not confirm create-if-absent semantics"));
    assert_eq!(split_captured_requests(&server.await?).len(), 3);
    Ok(())
}

#[tokio::test]
async fn cos_forbid_overwrite_is_create_if_absent_and_signed() -> Result<()> {
    let _env = EnvGuard::new();
    let body =
        "<Error><Code>FileAlreadyExists</Code><Message>File already exists.</Message></Error>";
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        format!(
            "HTTP/1.1 409 Conflict\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint))
        .await?
        .with_lock_detection_mode(LockDetectionMode::XCosForbidOverwrite);
    assert!(store.put_if_absent("lock", Bytes::new()).await?);
    assert!(!store.put_if_absent("lock", Bytes::new()).await?);

    for (head, body) in split_captured_requests(&server.await?) {
        assert!(head.starts_with("PUT "));
        assert!(head.contains("x-cos-forbid-overwrite: true"));
        assert!(!head.contains("if-none-match:"));
        verify_wire_signature(
            &format!("{head}\r\n\r\n{}", String::from_utf8_lossy(&body)),
            "fixture-secret",
        );
        assert!(body.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn cos_lock_acquisition_rejects_ignored_forbid_overwrite_header() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint))
        .await?
        .with_lock_detection_mode(LockDetectionMode::XCosForbidOverwrite);
    let error = HeldLock::acquire(&store, "backup/").await.err().unwrap();
    assert!(format!("{error:#}").contains("storage does not confirm create-if-absent semantics"));

    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 3);
    for (head, body) in requests.iter().take(2) {
        assert!(head.starts_with("PUT "));
        assert!(head.contains("x-cos-forbid-overwrite: true"));
        assert!(!head.contains("if-none-match:"));
        verify_wire_signature(
            &format!("{head}\r\n\r\n{}", String::from_utf8_lossy(body)),
            "fixture-secret",
        );
        assert_eq!(body.len(), 64);
    }
    Ok(())
}

#[tokio::test]
async fn dangerously_skip_probe_checks_metadata_without_conditional_puts() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 31\r\nETag: \"probe\"\r\nConnection: close\r\n\r\n"
            .into(),
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint))
        .await?
        .with_lock_detection_mode(LockDetectionMode::DangerouslySkip);
    let error = probe_metadata(&store, "backup/", "backup/stream.encrypted", false)
        .await
        .expect_err("the fixture omits the probed user metadata");
    assert!(format!("{error:#}").contains("did not preserve configured metadata"));

    let requests = split_captured_requests(&server.await?);
    assert_eq!(requests.len(), 3);
    assert!(requests[0].0.starts_with("PUT "));
    assert!(!requests[0].0.contains("if-none-match:"));
    assert!(!requests[0].0.contains("x-cos-forbid-overwrite:"));
    Ok(())
}

#[tokio::test]
async fn metadata_probe_rejects_matching_size_without_metadata() -> Result<()> {
    let _env = EnvGuard::new();
    let responses = [
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\nContent-Length: 31\r\nETag: \"probe\"\r\nConnection: close\r\n\r\n",
        "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let (endpoint, server) = fixture_sequence(responses).await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = probe_metadata(&store, "backup/", "backup/stream.encrypted", false)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("missing="));
    assert_eq!(split_captured_requests(&server.await?).len(), 3);
    Ok(())
}

#[tokio::test]
async fn lock_acquisition_surfaces_unexpected_server_errors_on_duplicate_put() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let error = HeldLock::acquire(&store, "backup/").await.err().unwrap();
    assert!(format!("{error:#}").contains("HTTP 503"));
    assert_eq!(split_captured_requests(&server.await?).len(), 3);
    Ok(())
}
