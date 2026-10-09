use super::support::{config, fixture, fixture_sequence, verify_wire_signature, EnvGuard};
use anyhow::Result;
use snapshot_to_s3::{
    s3::{is_retryable, HttpStore},
    store::ObjectStore,
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
        .probe_capabilities("backups/snapshot")
        .await
        .expect_err("a provider ignoring metadata must fail its capability probe");
    assert!(format!("{error:#}").contains("metadata prefix"));
    let requests = String::from_utf8(server.await?).expect("fixture requests are UTF-8");
    assert!(requests.contains("/fixture-bucket/backups/snapshot/.snapshot-to-s3-probes/"));
    assert!(requests.contains("/.lock HTTP/1.1"));
    assert!(requests.contains("x-fixture-meta-http-store-capability:"));
    assert!(requests.contains("DELETE /fixture-bucket/backups/snapshot/.snapshot-to-s3-probes/"));
    Ok(())
}
