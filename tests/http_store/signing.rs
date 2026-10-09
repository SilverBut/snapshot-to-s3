use super::support::{
    config, endpoint_host_from_request, fixture, verify_wire_signature, EnvGuard,
};
use anyhow::Result;
use bytes::Bytes;
use snapshot_to_s3::{model::MetadataMap, s3::HttpStore, store::ObjectStore};

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
