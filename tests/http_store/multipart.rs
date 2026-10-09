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
