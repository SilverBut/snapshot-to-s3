use super::support::{config, fixture, fixture_sequence, split_captured_requests, EnvGuard};
use anyhow::Result;
use snapshot_to_s3::{
    s3::{is_definite_rejection, is_retryable, HttpStore},
    store::{upload_parts, ObjectStore, Part, UploadLimits},
};

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
    let mut reader = std::io::Cursor::new(ciphertext.clone());
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
    assert!(result.ended);
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
async fn conditional_put_reports_creation_and_both_conflict_statuses() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture_sequence(vec![
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 412 Precondition Failed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        "HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
    ])
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    assert!(
        store
            .put_if_absent("lock", bytes::Bytes::from_static(b"token"))
            .await?
    );
    assert!(
        !store
            .put_if_absent("lock", bytes::Bytes::from_static(b"token"))
            .await?
    );
    assert!(
        !store
            .put_if_absent("lock", bytes::Bytes::from_static(b"token"))
            .await?
    );
    let mut server = server;
    let captured = tokio::time::timeout(std::time::Duration::from_secs(1), &mut server)
        .await
        .expect("conditional PUT requests must reach the fixture")?;
    assert_eq!(split_captured_requests(&captured).len(), 3);
    Ok(())
}

#[tokio::test]
async fn create_upload_requires_and_returns_a_nonempty_server_id() -> Result<()> {
    let _env = EnvGuard::new();
    let bodies = [
        "<InitiateMultipartUploadResult><UploadId>upload-123</UploadId></InitiateMultipartUploadResult>",
        "<InitiateMultipartUploadResult><UploadId></UploadId></InitiateMultipartUploadResult>",
    ];
    let (endpoint, server) = fixture_sequence(
        bodies
            .into_iter()
            .map(|body| {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            })
            .collect(),
    )
    .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    assert_eq!(
        store.create_upload("object", &Default::default()).await?,
        "upload-123"
    );
    let error = store
        .create_upload("object", &Default::default())
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "CreateMultipartUpload response contained an empty upload ID"
    );
    assert_eq!(split_captured_requests(&server.await?).len(), 2);
    Ok(())
}

#[tokio::test]
async fn completion_checks_nonempty_unique_ordered_part_number_boundaries() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) = fixture("HTTP/1.1 200 OK\r\nContent-Length: 32\r\nConnection: close\r\n\r\n<CompleteMultipartUploadResult/>").await?;
    let store = HttpStore::new(config(endpoint)).await?;
    let invalid = [
        vec![],
        vec![Part {
            number: 0,
            etag: "zero".into(),
        }],
        vec![Part {
            number: 10_001,
            etag: "too-large".into(),
        }],
        vec![
            Part {
                number: 2,
                etag: "two".into(),
            },
            Part {
                number: 1,
                etag: "one".into(),
            },
        ],
        vec![
            Part {
                number: 1,
                etag: "first".into(),
            },
            Part {
                number: 1,
                etag: "duplicate".into(),
            },
        ],
    ];
    for parts in invalid {
        let error = store
            .complete_upload("object", "upload", &parts)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "multipart completion requires ordered unique part numbers in 1..=10000"
        );
    }
    store
        .complete_upload(
            "object",
            "upload",
            &[
                Part {
                    number: 1,
                    etag: "first".into(),
                },
                Part {
                    number: 10_000,
                    etag: "last".into(),
                },
            ],
        )
        .await?;
    let requests = split_captured_requests(&server.await?);
    assert_eq!(
        requests.len(),
        1,
        "invalid input must not reach the endpoint"
    );
    let xml = String::from_utf8(requests[0].1.clone())?;
    assert!(xml.contains("<PartNumber>1</PartNumber>"));
    assert!(xml.contains("<PartNumber>10000</PartNumber>"));
    Ok(())
}

#[tokio::test]
async fn abort_upload_sends_delete_for_the_exact_upload_id() -> Result<()> {
    let _env = EnvGuard::new();
    let (endpoint, server) =
        fixture("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
    let store = HttpStore::new(config(endpoint)).await?;
    store.abort_upload("stream/key", "id +/雪").await?;
    let mut server = server;
    let captured = tokio::time::timeout(std::time::Duration::from_secs(1), &mut server)
        .await
        .expect("abort must send a DELETE request")?;
    let request = String::from_utf8(captured)?;
    assert!(request
        .starts_with("DELETE /fixture-bucket/stream/key?uploadId=id%20%2B%2F%E9%9B%AA HTTP/1.1"));
    Ok(())
}
