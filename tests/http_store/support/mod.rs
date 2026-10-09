mod env;
mod server;
mod sigv4;

pub(crate) use env::EnvGuard;
pub(crate) use server::{fixture, fixture_sequence, paced_fixture, read_completion_request};
pub(crate) use sigv4::{
    endpoint_host_from_request, split_captured_requests, verify_wire_signature,
};

use snapshot_to_s3::s3::{HttpConfig, HttpPolicy};
use std::time::Duration;

/// Builds path-style S3 configuration for the local fixture endpoint.
pub(crate) fn config(endpoint: String) -> HttpConfig {
    HttpConfig {
        bucket: "fixture-bucket".into(),
        endpoint: Some(endpoint),
        region: "us-east-1".into(),
        metadata_prefix: "x-fixture-meta".into(),
        signing_service: "s3".into(),
        path_style: true,
    }
}

/// Uses short throughput and retry windows to exercise failures quickly.
pub(crate) fn fast_policy() -> HttpPolicy {
    HttpPolicy {
        throughput_window: Duration::from_millis(100),
        minimum_bytes_per_window: 8,
        control_timeout: Duration::from_millis(25),
        get_retries: 2,
        retry_backoff: Duration::from_millis(1),
    }
}

/// Builds a closed GET response with explicit length, ETag, and optional range.
pub(crate) fn get_reply(range: Option<&str>, length: usize, etag: &str, body: &str) -> String {
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
