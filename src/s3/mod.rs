//! S3-compatible object store over HTTP with SigV4 signing.
//!
//! Only the operations backup and restore need are implemented. Transfers
//! fail on low recent throughput rather than on total duration, and GETs
//! resume with ETag-pinned range requests.

mod credentials;
mod error;
mod get;
mod object_store;
mod policy;
mod probe;
mod sigv4;
mod throughput;

pub use error::{is_definite_rejection, is_retryable};
pub use policy::HttpPolicy;
pub use probe::PROBE_NAMESPACE;

use crate::model::MetadataMap;
use crate::store::FilePart;
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use chrono::Utc;
use credentials::Credentials;
use error::HttpStatusFailure;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, Response, Url};
use sha2::{Digest, Sha256};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use throughput::ThroughputGuard;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LockDetectionMode {
    #[default]
    #[value(name = "if-none-match")]
    IfNoneMatch,
    #[value(name = "x-cos-forbid-overwrite")]
    XCosForbidOverwrite,
    #[value(name = "dangerously-skip")]
    DangerouslySkip,
}

/// Largest XML or error response body that is read into memory.
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// Request bodies are streamed in chunks of this size to measure progress.
const UPLOAD_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub bucket: String,
    /// Defaults to the AWS endpoint of `region`.
    pub endpoint: Option<String>,
    pub region: String,
    /// User metadata header prefix, normally `x-amz-meta`.
    pub metadata_prefix: String,
    pub signing_service: String,
    pub path_style: bool,
}

#[derive(Clone)]
pub struct HttpStore {
    config: HttpConfig,
    client: reqwest::Client,
    credentials: Arc<Credentials>,
    metadata_header_prefix: String,
    endpoint: Url,
    policy: HttpPolicy,
    lock_detection_mode: LockDetectionMode,
}

impl HttpStore {
    /// Creates a store with the policy from the environment.
    pub async fn new(config: HttpConfig) -> Result<Self> {
        Self::new_with_policy(config, HttpPolicy::from_env()?).await
    }

    pub async fn new_with_policy(config: HttpConfig, policy: HttpPolicy) -> Result<Self> {
        policy.validate()?;
        if config.bucket.is_empty()
            || config.bucket.contains(['/', '@', '?', '#', '\0'])
            || config.region.trim().is_empty()
            || config.signing_service.trim().is_empty()
        {
            bail!("invalid HTTP object-store configuration");
        }
        let metadata_header_prefix = normalize_metadata_prefix(&config.metadata_prefix)?;
        let endpoint = endpoint_url(&config)?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("create S3 HTTP client")?;
        let credentials = Arc::new(Credentials::from_env()?);
        Ok(Self {
            config,
            client,
            credentials,
            metadata_header_prefix,
            endpoint,
            policy,
            lock_detection_mode: LockDetectionMode::default(),
        })
    }

    pub fn with_lock_detection_mode(mut self, mode: LockDetectionMode) -> Self {
        self.lock_detection_mode = mode;
        self
    }

    fn conditional_put_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        let (name, value) = match self.lock_detection_mode {
            LockDetectionMode::IfNoneMatch => ("if-none-match", "*"),
            LockDetectionMode::XCosForbidOverwrite => ("x-cos-forbid-overwrite", "true"),
            LockDetectionMode::DangerouslySkip => {
                bail!("conditional PUT is disabled by dangerously-skip mode")
            }
        };
        put_header(&mut headers, name, value)?;
        Ok(headers)
    }

    fn lock_condition_description(&self) -> &'static str {
        match self.lock_detection_mode {
            LockDetectionMode::IfNoneMatch => "If-None-Match: *",
            LockDetectionMode::XCosForbidOverwrite => "x-cos-forbid-overwrite: true",
            LockDetectionMode::DangerouslySkip => "dangerously-skip",
        }
    }

    fn object_url(&self, key: &str, query: &[(String, String)]) -> Result<Url> {
        let mut url = self.endpoint.clone();
        let host = url
            .host_str()
            .context("S3 endpoint has no host")?
            .to_owned();
        if !self.config.path_style {
            url.set_host(Some(&format!("{}.{}", self.config.bucket, host)))
                .context("invalid virtual-hosted S3 bucket name")?;
        }
        let base = url.path().trim_end_matches('/');
        let mut resource = base.to_owned();
        if self.config.path_style {
            resource.push('/');
            resource.push_str(&sigv4::uri_encode(&self.config.bucket));
            if !key.is_empty() {
                resource.push('/');
            }
        } else {
            resource.push('/');
        }
        resource.push_str(&sigv4::encode_key(key));
        url.set_path(&resource);
        if query.is_empty() {
            url.set_query(None);
        } else {
            // SigV4 orders parameters by encoded name, then value.
            let mut pairs = query
                .iter()
                .map(|(k, v)| (sigv4::uri_encode(k), sigv4::uri_encode(v)))
                .collect::<Vec<_>>();
            pairs.sort();
            let query = pairs
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            url.set_query(Some(&query));
        }
        Ok(url)
    }

    /// Signs and sends one request. Control requests (HEAD, DELETE, LIST)
    /// have a whole-request timeout; others use the throughput guard.
    async fn send_signed(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response> {
        self.send_signed_body(method, key, query, headers, RequestBody::Bytes(body))
            .await
    }

    /// [`Self::send_signed`] for a body in memory or in the part temporary file.
    async fn send_signed_body(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        mut headers: HeaderMap,
        body: RequestBody,
    ) -> Result<Response> {
        let url = self.object_url(key, query)?;
        let body_len = body.len();
        tracing::debug!(%method, %key, body_bytes = body_len, "S3 request");
        let scope = sigv4::Scope {
            region: &self.config.region,
            service: &self.config.signing_service,
        };
        let payload_sha256 = match &body {
            RequestBody::Bytes(bytes) => Sha256::digest(bytes).into(),
            RequestBody::File(part) => part.sha256(),
        };
        sigv4::sign(
            &mut headers,
            &method,
            &url,
            payload_sha256,
            &self.credentials,
            &scope,
            Utc::now(),
        )?;
        let control = method == Method::HEAD
            || method == Method::DELETE
            || (method == Method::GET && !query.is_empty());
        let transfer_body = method == Method::PUT || method == Method::POST;
        let mut request = self.client.request(method, url).headers(headers);
        if control {
            request = request.timeout(self.policy.control_timeout);
        }
        let progress = Arc::new(AtomicU64::new(0));
        if transfer_body {
            request = request.header(reqwest::header::CONTENT_LENGTH, body_len);
            if body_len > 0 {
                request = request.body(match body {
                    RequestBody::Bytes(bytes) => counted_body(bytes, progress.clone()),
                    RequestBody::File(part) => {
                        counted_file_body(part.reader().await?, progress.clone())
                    }
                });
            }
        }
        let started = std::time::Instant::now();
        let result = if control {
            request.send().await.context("send signed S3 request")
        } else {
            ThroughputGuard::new(&self.policy)
                .wait(request.send(), Some(&progress))
                .await?
                .context("send signed S3 request")
        };
        match &result {
            Ok(response) => tracing::debug!(
                status = %response.status(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                %key,
                "S3 response"
            ),
            Err(error) => tracing::debug!(%key, "S3 request failed: {error:#}"),
        }
        result
    }

    async fn response_error(&self, operation: &str, response: Response) -> anyhow::Error {
        let status = response.status();
        let header_code = response
            .headers()
            .get("x-amz-error-code")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let failure = match self.read_body(response).await {
            Ok(body) => HttpStatusFailure::from_body(operation, status, header_code, &body),
            Err(error) => {
                let mut failure = HttpStatusFailure::from_body(operation, status, header_code, &[]);
                failure.detail = Some(format!("failed to read error body: {error:#}"));
                failure
            }
        };
        failure.into()
    }

    async fn require_success(&self, operation: &str, response: Response) -> Result<Response> {
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(self.response_error(operation, response).await)
        }
    }

    async fn read_response(&self, operation: &str, response: Response) -> Result<Vec<u8>> {
        let response = self.require_success(operation, response).await?;
        self.read_body(response).await.with_context(|| {
            format!("{operation}: response exceeds {MAX_RESPONSE_BYTES} bytes or failed")
        })
    }

    async fn read_body(&self, mut response: Response) -> Result<Vec<u8>> {
        let mut guard = ThroughputGuard::new(&self.policy);
        let mut bytes = Vec::new();
        while let Some(chunk) = guard.wait(response.chunk(), None).await?? {
            guard.add(chunk.len() as u64);
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                bail!("HTTP response exceeds {MAX_RESPONSE_BYTES}-byte limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn metadata_headers(&self, metadata: &MetadataMap) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        for (key, value) in metadata {
            if key.is_empty()
                || !key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                bail!("invalid S3 metadata key: {key:?}");
            }
            let name = format!("{}-{}", self.metadata_header_prefix, key);
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid metadata header name {name:?}"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid value for metadata key {key:?}"))?;
            headers.insert(name, value);
        }
        Ok(headers)
    }

    fn metadata_from_headers(&self, headers: &HeaderMap) -> MetadataMap {
        let prefix = format!("{}-", self.metadata_header_prefix);
        headers
            .iter()
            .filter_map(|(name, value)| {
                name.as_str()
                    .strip_prefix(&prefix)
                    .and_then(|key| value.to_str().ok().map(|value| (key.into(), value.into())))
            })
            .collect()
    }
}

fn endpoint_url(config: &HttpConfig) -> Result<Url> {
    let text = config.endpoint.clone().unwrap_or_else(|| {
        if config.region == "us-east-1" {
            "https://s3.amazonaws.com".to_owned()
        } else {
            format!("https://s3.{}.amazonaws.com", config.region)
        }
    });
    let endpoint = Url::parse(&text).context("invalid S3 endpoint URL")?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        bail!("S3 endpoint must be an HTTP(S) URL without credentials, query, or fragment");
    }
    Ok(endpoint)
}

fn normalize_metadata_prefix(prefix: &str) -> Result<String> {
    let prefix = prefix.trim_end_matches('-').to_ascii_lowercase();
    if prefix.is_empty()
        || !prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("metadata_prefix must be a valid HTTP header-name prefix");
    }
    Ok(prefix)
}

/// A request body in memory or in the part temporary file.
pub(super) enum RequestBody {
    Bytes(Bytes),
    File(FilePart),
}

impl RequestBody {
    fn len(&self) -> u64 {
        match self {
            Self::Bytes(bytes) => bytes.len() as u64,
            Self::File(part) => part.len(),
        }
    }
}

/// Streams a part from the part temporary file, counting bytes handed to
/// the connection.
fn counted_file_body(
    reader: impl tokio::io::AsyncRead + Send + Sync + Unpin + 'static,
    progress: Arc<AtomicU64>,
) -> reqwest::Body {
    use futures_util::StreamExt;
    let stream =
        tokio_util::io::ReaderStream::with_capacity(reader, UPLOAD_CHUNK_BYTES).map(move |chunk| {
            if let Ok(chunk) = &chunk {
                progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            chunk
        });
    reqwest::Body::wrap_stream(stream)
}

/// A streamed request body that counts bytes handed to the connection.
fn counted_body(body: Bytes, progress: Arc<AtomicU64>) -> reqwest::Body {
    let stream = futures_util::stream::unfold(body, move |mut remaining| {
        let progress = progress.clone();
        async move {
            if remaining.is_empty() {
                return None;
            }
            let chunk = remaining.split_to(remaining.len().min(UPLOAD_CHUNK_BYTES));
            progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            Some((Ok::<_, io::Error>(chunk), remaining))
        }
    });
    reqwest::Body::wrap_stream(stream)
}

fn put_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_bytes(name.as_bytes()).context("invalid HTTP header name")?,
        HeaderValue::from_str(value)
            .with_context(|| format!("invalid HTTP header value for {name}"))?,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(endpoint: Option<&str>, region: &str) -> HttpConfig {
        HttpConfig {
            bucket: "bucket".into(),
            endpoint: endpoint.map(str::to_owned),
            region: region.into(),
            metadata_prefix: "X-Test-Meta---".into(),
            signing_service: "s3".into(),
            path_style: true,
        }
    }

    #[tokio::test]
    async fn missing_credentials_fail_when_the_store_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        let _env = crate::testing::ScopedEnv::new(&[
            ("AWS_ACCESS_KEY_ID", None),
            ("AWS_SECRET_ACCESS_KEY", None),
            ("AWS_SHARED_CREDENTIALS_FILE", path.to_str()),
        ]);
        let error = HttpStore::new_with_policy(
            config(Some("http://127.0.0.1:1"), "us-east-1"),
            HttpPolicy::default(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(
            error.to_string(),
            "no AWS credentials found; set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY or configure a shared credentials file"
        );
    }

    #[test]
    fn endpoint_defaults_and_rejects_each_unsupported_component() {
        assert_eq!(
            endpoint_url(&config(None, "us-east-1")).unwrap().as_str(),
            "https://s3.amazonaws.com/"
        );
        assert_eq!(
            endpoint_url(&config(None, "eu-west-2")).unwrap().as_str(),
            "https://s3.eu-west-2.amazonaws.com/"
        );
        for endpoint in [
            "ftp://example.test",
            "file:///tmp",
            "http://user@example.test",
            "http://user:pass@example.test",
            "http://:pass@example.test",
            "http://example.test/path?query=1",
            "http://example.test/path#fragment",
        ] {
            let error = endpoint_url(&config(Some(endpoint), "us-east-1")).unwrap_err();
            assert!(
                error.to_string().contains("S3 endpoint"),
                "{endpoint}: {error:#}"
            );
        }
    }

    #[test]
    fn metadata_prefix_is_normalized_and_rejects_empty_or_invalid_names() {
        assert_eq!(
            normalize_metadata_prefix("X-Test-Meta---").unwrap(),
            "x-test-meta"
        );
        for prefix in ["", "---", "bad_prefix", "bad prefix"] {
            let error = normalize_metadata_prefix(prefix).unwrap_err().to_string();
            assert_eq!(
                error,
                "metadata_prefix must be a valid HTTP header-name prefix"
            );
        }
    }
}
