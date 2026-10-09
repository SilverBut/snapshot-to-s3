use super::support::{config, fixture, EnvGuard};
use anyhow::Result;
use bytes::Bytes;
use snapshot_to_s3::{
    s3::{is_definite_rejection, is_retryable, HttpStore},
    store::ObjectStore,
};
use std::io;

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
