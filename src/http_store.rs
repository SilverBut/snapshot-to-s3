use crate::{
    model::{MetadataMap, Reader},
    store::{ObjectHead, ObjectStore, Part},
};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use quick_xml::de::from_reader;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, ETAG},
    Method, Response, StatusCode, Url,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    io::{self, Read},
    path::PathBuf,
    pin::Pin,
    task::{Context as TaskContext, Poll},
    time::Duration,
};
use tokio::{io::AsyncRead, sync::Mutex};
use tokio_util::io::StreamReader;

const MAX_XML_BYTES: usize = 1024 * 1024;
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug)]
pub struct HttpConfig {
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub metadata_prefix: String,
    pub signing_service: String,
    pub path_style: bool,
}

#[derive(Clone)]
struct Credentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

struct CredentialProvider {
    static_credentials: Option<Credentials>,
    cache: Mutex<Option<Credentials>>,
    client: reqwest::Client,
    metadata_disabled: bool,
}

pub struct HttpStore {
    config: HttpConfig,
    client: reqwest::Client,
    credentials: CredentialProvider,
    metadata_header_prefix: String,
    endpoint: Url,
}

#[derive(Debug, Deserialize)]
struct S3ErrorXml {
    #[serde(rename = "Code")]
    code: Option<String>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListXml {
    #[serde(rename = "IsTruncated", default)]
    is_truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next_continuation_token: Option<String>,
    #[serde(rename = "Contents", default)]
    contents: Vec<ListContentXml>,
}

#[derive(Debug, Deserialize)]
struct ListContentXml {
    #[serde(rename = "Key")]
    key: String,
}

#[derive(Debug, Deserialize)]
struct CreateMultipartXml {
    #[serde(rename = "UploadId")]
    upload_id: String,
}

#[derive(Debug)]
struct HttpStatusFailure {
    operation: String,
    status: StatusCode,
    code: Option<String>,
    message: Option<String>,
    detail: Option<String>,
    definite_rejection: bool,
}

impl std::fmt::Display for HttpStatusFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: HTTP {}", self.operation, self.status)?;
        if let Some(code) = &self.code {
            write!(formatter, " ({code})")?;
        }
        if let Some(message) = &self.message {
            write!(formatter, ": {message}")?;
        }
        if let Some(detail) = &self.detail {
            write!(formatter, "; {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for HttpStatusFailure {}

/// Whether a failed HTTP request may be retried with the identical request body.
///
/// Callers should restrict retries to operations whose protocol semantics permit it.
pub fn is_retryable(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(failure) = cause.downcast_ref::<HttpStatusFailure>() {
            return failure.status == StatusCode::REQUEST_TIMEOUT
                || failure.status == StatusCode::TOO_MANY_REQUESTS
                || failure.status.is_server_error();
        }
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            if error.is_timeout() || error.is_connect() {
                return true;
            }
        }
        if let Some(error) = cause.downcast_ref::<io::Error>() {
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut
                    | io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
                    | io::ErrorKind::AddrNotAvailable
                    | io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::WouldBlock
            ) {
                return true;
            }
        }
    }
    false
}

/// Whether an error proves that a mutating HTTP request was rejected.
///
/// Network failures, 408/5xx responses, and malformed successful responses do not
/// establish whether the service accepted a mutation.
pub fn is_definite_rejection(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<HttpStatusFailure>()
            .is_some_and(|failure| failure.definite_rejection)
    })
}

impl HttpStore {
    pub async fn new(config: HttpConfig) -> Result<Self> {
        if config.bucket.is_empty()
            || config.bucket.contains(['/', '@', '?', '#', '\0'])
            || config.region.trim().is_empty()
            || config.signing_service.trim().is_empty()
        {
            bail!("invalid HTTP object-store configuration");
        }
        let prefix = normalize_metadata_prefix(&config.metadata_prefix)?;
        let endpoint_text = config.endpoint.clone().unwrap_or_else(|| {
            if config.region == "us-east-1" {
                "https://s3.amazonaws.com".to_owned()
            } else {
                format!("https://s3.{}.amazonaws.com", config.region)
            }
        });
        let endpoint = Url::parse(&endpoint_text).context("invalid S3 endpoint URL")?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || endpoint.username() != ""
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            bail!("S3 endpoint must be an HTTP(S) URL without credentials, query, or fragment");
        }

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .context("create S3 HTTP client")?;
        let static_credentials = load_static_credentials()?;
        let metadata_disabled = env::var("AWS_EC2_METADATA_DISABLED")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if static_credentials.is_none() && metadata_disabled {
            bail!("no AWS credentials found and EC2 metadata credentials are disabled");
        }
        Ok(Self {
            config,
            client: client.clone(),
            credentials: CredentialProvider {
                static_credentials,
                cache: Mutex::new(None),
                client,
                metadata_disabled,
            },
            metadata_header_prefix: prefix,
            endpoint,
        })
    }

    /// Verify atomic conditional creation and configured metadata round-trip beneath a key prefix.
    pub async fn validate_conditional_put(&self, prefix: &str) -> Result<()> {
        let mut random = [0u8; 24];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut random);
        let namespace = ".__snapshot-to-s3-condition-check";
        let prefix = prefix.trim_end_matches('/');
        let key = if prefix.is_empty() {
            format!("{namespace}/{}/.lock", hex::encode(random))
        } else {
            format!("{prefix}/{namespace}/{}/.lock", hex::encode(random))
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("if-none-match"),
            HeaderValue::from_static("*"),
        );
        let first = self
            .send_signed(Method::PUT, &key, &[], headers.clone(), Bytes::new())
            .await;
        let first = match first {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(self
                .response_error("conditional-put capability probe (initial put)", response)
                .await),
            Err(error) => Err(error.context("conditional-put capability probe (initial put)")),
        };
        if let Err(error) = first {
            return Err(attach_probe_cleanup(error, self.delete(&key).await));
        }
        let second = self
            .send_signed(Method::PUT, &key, &[], headers, Bytes::new())
            .await;
        let second = match second {
            Ok(response) => {
                let supported = matches!(
                    response.status(),
                    StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED
                );
                if supported {
                    Ok(())
                } else if response.status().is_success() {
                    Err(anyhow!(
                        "S3 endpoint ignored If-None-Match: *; atomic locking is unavailable"
                    ))
                } else {
                    Err(self
                        .response_error(
                            "conditional-put capability probe (duplicate put)",
                            response,
                        )
                        .await)
                }
            }
            Err(error) => Err(error.context("conditional-put capability probe (duplicate put)")),
        };
        let cleanup = self.delete(&key).await;
        match (second, cleanup) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), Err(error)) => {
                return Err(error.context("conditional-put capability probe cleanup failed"))
            }
            (Err(error), Err(cleanup_error)) => {
                return Err(error.context(format!(
                    "conditional-put probe cleanup also failed: {cleanup_error:#}"
                )))
            }
        }

        let marker = hex::encode(random);
        let metadata = MetadataMap::from([("http-store-capability".into(), marker)]);
        let data = Bytes::from_static(b"snapshot-to-s3 capability probe");
        if let Err(error) = self.put(&key, data.clone(), &metadata).await {
            return Err(attach_probe_cleanup(error, self.delete(&key).await));
        }
        let verification = match self.head(&key).await {
            Ok(Some(head)) if head.size == data.len() as u64 && head.metadata == metadata => Ok(()),
            Ok(Some(head)) => Err(anyhow!(
                "S3 endpoint did not preserve configured metadata prefix on capability object (expected {:?}, got {:?})",
                metadata,
                head.metadata
            )),
            Ok(None) => Err(anyhow!(
                "S3 endpoint did not return the capability object from HEAD"
            )),
            Err(error) => Err(error.context("HEAD metadata capability object")),
        };
        let cleanup = self.delete(&key).await;
        match (verification, cleanup) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error.context("metadata capability probe cleanup failed")),
            (Err(error), Err(cleanup_error)) => Err(error.context(format!(
                "metadata capability probe cleanup also failed: {cleanup_error:#}"
            ))),
        }
    }

    async fn credentials(&self) -> Result<Credentials> {
        if let Some(credentials) = &self.credentials.static_credentials {
            return Ok(credentials.clone());
        }
        let mut cached = self.credentials.cache.lock().await;
        if let Some(credentials) = cached.as_ref() {
            if credentials
                .expires_at
                .map(|expiration| expiration > Utc::now() + chrono::Duration::minutes(5))
                .unwrap_or(false)
            {
                return Ok(credentials.clone());
            }
        }
        if self.credentials.metadata_disabled {
            bail!("no AWS credentials found and EC2 metadata credentials are disabled");
        }
        let credentials = fetch_imds_credentials(&self.credentials.client).await?;
        *cached = Some(credentials.clone());
        Ok(credentials)
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
        let resource = if self.config.path_style {
            format!(
                "{}/{}{}",
                base,
                encode_path_segment(&self.config.bucket),
                if key.is_empty() {
                    String::new()
                } else {
                    format!("/{}", encode_key(key))
                }
            )
        } else if key.is_empty() {
            format!("{}/", base)
        } else {
            format!("{}/{}", base, encode_key(key))
        };
        url.set_path(&resource);
        if !query.is_empty() {
            let mut pairs = query
                .iter()
                .map(|(k, v)| (aws_encode(k), aws_encode(v)))
                .collect::<Vec<_>>();
            pairs.sort();
            url.set_query(Some(
                &pairs
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("&"),
            ));
        } else {
            url.set_query(None);
        }
        Ok(url)
    }

    async fn send_signed(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        mut headers: HeaderMap,
        body: Bytes,
    ) -> Result<Response> {
        let credentials = self.credentials().await?;
        let url = self.object_url(key, query)?;
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let payload_hash = hex::encode(Sha256::digest(&body));
        put_header(&mut headers, "x-amz-date", &amz_date)?;
        put_header(&mut headers, "x-amz-content-sha256", &payload_hash)?;
        if let Some(token) = &credentials.session_token {
            put_header(&mut headers, "x-amz-security-token", token)?;
        }
        let host = host_header(&url)?;
        let (canonical_headers, signed_headers) = canonical_headers(&headers, &host)?;
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            url.path(),
            url.query().unwrap_or(""),
            canonical_headers,
            signed_headers,
            payload_hash
        );
        let scope = format!(
            "{date}/{}/{}/aws4_request",
            self.config.region, self.config.signing_service
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let signing_key = signing_key(
            credentials.secret_key.as_bytes(),
            &date,
            &self.config.region,
            &self.config.signing_service,
        );
        let mut mac =
            HmacSha256::new_from_slice(&signing_key).expect("HMAC accepts keys of any size");
        mac.update(string_to_sign.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            credentials.access_key
        );
        put_header(&mut headers, "authorization", &authorization)?;
        let mut request = self.client.request(method, url);
        for (name, value) in headers.iter() {
            request = request.header(name, value);
        }
        if !body.is_empty() {
            request = request.body(body);
        }
        request.send().await.context("send signed S3 request")
    }

    async fn response_error(&self, operation: &str, response: Response) -> anyhow::Error {
        let status = response.status();
        let header_code = response
            .headers()
            .get("x-amz-error-code")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = match read_limited(response, MAX_XML_BYTES).await {
            Ok(body) => body,
            Err(error) => {
                return anyhow::Error::new(HttpStatusFailure {
                    operation: operation.to_owned(),
                    status,
                    code: header_code,
                    message: None,
                    detail: Some(format!("failed to read error body: {error:#}")),
                    definite_rejection: is_definite_rejection_status(status),
                })
            }
        };
        let parsed = parse_s3_error(&body).ok();
        let code = header_code.or_else(|| parsed.as_ref().and_then(|error| error.code.clone()));
        let message = parsed.and_then(|error| error.message);
        let detail = if code.is_none() && message.is_none() && !body.is_empty() {
            Some(format!("response body: {}", String::from_utf8_lossy(&body)))
        } else {
            None
        };
        anyhow::Error::new(HttpStatusFailure {
            operation: operation.to_owned(),
            status,
            code,
            message,
            detail,
            definite_rejection: is_definite_rejection_status(status),
        })
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
        read_limited(response, MAX_XML_BYTES)
            .await
            .with_context(|| {
                format!("{operation}: response exceeds {MAX_XML_BYTES} bytes or failed")
            })
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

#[async_trait]
impl ObjectStore for HttpStore {
    async fn head(&self, key: &str) -> Result<Option<ObjectHead>> {
        let response = self
            .send_signed(Method::HEAD, key, &[], HeaderMap::new(), Bytes::new())
            .await
            .with_context(|| format!("HEAD s3://{}/{}", self.config.bucket, key))?;
        if response.status() == StatusCode::NOT_FOUND {
            let code = response
                .headers()
                .get("x-amz-error-code")
                .and_then(|v| v.to_str().ok());
            if code.is_none()
                || matches!(
                    code,
                    Some("NoSuchKey" | "NoSuchBucket" | "NotFound" | "NoSuchObject")
                )
            {
                return Ok(None);
            }
            return Err(self.response_error("HEAD object", response).await);
        }
        let response = self.require_success("HEAD object", response).await?;
        let size = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .context("HEAD object response omitted Content-Length")?
            .to_str()
            .context("invalid HEAD Content-Length")?
            .parse::<u64>()
            .context("invalid HEAD Content-Length")?;
        let etag = response
            .headers()
            .get(ETAG)
            .context("HEAD object response omitted ETag")?
            .to_str()
            .context("invalid HEAD ETag")?
            .to_owned();
        Ok(Some(ObjectHead {
            size,
            etag,
            metadata: self.metadata_from_headers(response.headers()),
        }))
    }

    async fn get(
        &self,
        key: &str,
        etag: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> Result<Reader> {
        if let Some((start, end)) = range {
            if start > end {
                bail!("invalid byte range: start exceeds end");
            }
        }
        let mut headers = HeaderMap::new();
        if let Some(etag) = etag {
            put_header(&mut headers, "if-match", etag)?;
        }
        if let Some((start, end)) = range {
            put_header(&mut headers, "range", &format!("bytes={start}-{end}"))?;
        }
        let response = self
            .send_signed(Method::GET, key, &[], headers, Bytes::new())
            .await
            .with_context(|| format!("GET s3://{}/{}", self.config.bucket, key))?;
        let expected_len = if let Some((start, end)) = range {
            if response.status() != StatusCode::PARTIAL_CONTENT {
                return Err(self.response_error("range GET object", response).await);
            }
            let content_range = response
                .headers()
                .get("content-range")
                .and_then(|value| value.to_str().ok())
                .context("range GET response omitted valid Content-Range")?;
            let expected_range = format!("bytes {start}-{end}/");
            let total = content_range
                .strip_prefix(&expected_range)
                .and_then(|total| total.parse::<u64>().ok())
                .context("range GET returned malformed or mismatched Content-Range")?;
            if total <= end {
                bail!(
                    "range GET Content-Range total {total} does not include requested end byte {end}"
                );
            }
            Some(end - start + 1)
        } else {
            if response.status() != StatusCode::OK {
                return Err(self.response_error("GET object", response).await);
            }
            None
        };
        let advertised_len = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .map(|value| {
                value
                    .to_str()
                    .context("invalid GET Content-Length")?
                    .parse::<u64>()
                    .context("invalid GET Content-Length")
            })
            .transpose()?;
        if let Some(expected) = expected_len {
            if advertised_len != Some(expected) {
                bail!(
                    "range GET Content-Length mismatch: expected {expected}, got {advertised_len:?}"
                );
            }
        }
        let expected_len = advertised_len.or(expected_len);
        let stream = response
            .bytes_stream()
            .map(|result| result.map_err(|error| io::Error::other(error.to_string())));
        let reader = StreamReader::new(stream);
        Ok(Box::new(CheckedReader {
            inner: Box::pin(reader),
            expected: expected_len,
            read: 0,
            failed: false,
        }))
    }

    async fn put(&self, key: &str, data: Bytes, metadata: &MetadataMap) -> Result<()> {
        let headers = self.metadata_headers(metadata)?;
        let response = self
            .send_signed(Method::PUT, key, &[], headers, data)
            .await
            .with_context(|| format!("PUT s3://{}/{}", self.config.bucket, key))?;
        self.require_success("PUT object", response).await?;
        Ok(())
    }

    async fn put_if_absent(&self, key: &str, data: Bytes) -> Result<bool> {
        let mut headers = HeaderMap::new();
        put_header(&mut headers, "if-none-match", "*")?;
        let response = self
            .send_signed(Method::PUT, key, &[], headers, data)
            .await
            .with_context(|| format!("conditional PUT s3://{}/{}", self.config.bucket, key))?;
        if matches!(
            response.status(),
            StatusCode::CONFLICT | StatusCode::PRECONDITION_FAILED
        ) {
            return Ok(false);
        }
        self.require_success("conditional PUT object", response)
            .await?;
        Ok(true)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let response = self
            .send_signed(Method::DELETE, key, &[], HeaderMap::new(), Bytes::new())
            .await
            .with_context(|| format!("DELETE s3://{}/{}", self.config.bucket, key))?;
        self.require_success("DELETE object", response).await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut continuation: Option<String> = None;
        let mut keys = Vec::new();
        loop {
            let mut query = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(token) = &continuation {
                query.push(("continuation-token".into(), token.clone()));
            }
            let response = self
                .send_signed(Method::GET, "", &query, HeaderMap::new(), Bytes::new())
                .await
                .context("list S3 objects")?;
            let bytes = self.read_response("list S3 objects", response).await?;
            let page: ListXml = from_reader(bytes.as_slice()).context("parse ListObjectsV2 XML")?;
            keys.extend(page.contents.into_iter().map(|entry| entry.key));
            if !page.is_truncated {
                return Ok(keys);
            }
            let next = page
                .next_continuation_token
                .context("truncated ListObjectsV2 response omitted continuation token")?;
            if continuation.as_ref() == Some(&next) {
                bail!("ListObjectsV2 repeated continuation token");
            }
            continuation = Some(next);
        }
    }

    async fn create_upload(&self, key: &str, metadata: &MetadataMap) -> Result<String> {
        let query = vec![("uploads".to_owned(), String::new())];
        let response = self
            .send_signed(
                Method::POST,
                key,
                &query,
                self.metadata_headers(metadata)?,
                Bytes::new(),
            )
            .await
            .context("create multipart upload")?;
        let bytes = self
            .read_response("create multipart upload", response)
            .await?;
        let result: CreateMultipartXml =
            from_reader(bytes.as_slice()).context("parse CreateMultipartUpload XML")?;
        if result.upload_id.is_empty() {
            bail!("CreateMultipartUpload response contained an empty upload ID");
        }
        Ok(result.upload_id)
    }

    async fn upload_part(
        &self,
        key: &str,
        upload: &str,
        number: u32,
        data: Bytes,
    ) -> Result<String> {
        if number == 0 || number > 10_000 {
            bail!("multipart part number must be between 1 and 10000");
        }
        let query = vec![
            ("partNumber".to_owned(), number.to_string()),
            ("uploadId".to_owned(), upload.to_owned()),
        ];
        let response = self
            .send_signed(Method::PUT, key, &query, HeaderMap::new(), data)
            .await
            .with_context(|| format!("upload multipart part {number}"))?;
        let response = self
            .require_success("upload multipart part", response)
            .await?;
        response
            .headers()
            .get(ETAG)
            .context("UploadPart response omitted ETag")?
            .to_str()
            .context("invalid UploadPart ETag")
            .map(str::to_owned)
    }

    async fn complete_upload(&self, key: &str, upload: &str, parts: &[Part]) -> Result<()> {
        if parts.is_empty()
            || parts
                .iter()
                .any(|part| part.number == 0 || part.number > 10_000)
            || parts
                .windows(2)
                .any(|window| window[0].number >= window[1].number)
        {
            bail!("multipart completion requires ordered unique part numbers in 1..=10000");
        }
        let mut xml = String::from("<CompleteMultipartUpload>");
        for part in parts {
            xml.push_str("<Part><PartNumber>");
            xml.push_str(&part.number.to_string());
            xml.push_str("</PartNumber><ETag>");
            xml.push_str(&xml_escape(&part.etag));
            xml.push_str("</ETag></Part>");
        }
        xml.push_str("</CompleteMultipartUpload>");
        let query = vec![("uploadId".to_owned(), upload.to_owned())];
        let response = self
            .send_signed(
                Method::POST,
                key,
                &query,
                HeaderMap::new(),
                Bytes::from(xml),
            )
            .await
            .context("complete multipart upload")?;
        let status = response.status();
        let bytes = read_limited(response, MAX_XML_BYTES)
            .await
            .context("read CompleteMultipartUpload response")?;
        if !status.is_success() {
            return Err(error_from_xml("complete multipart upload", status, &bytes));
        }
        let root = xml_root_name(&bytes).context("parse CompleteMultipartUpload response")?;
        if root == "Error" {
            return Err(error_from_xml("complete multipart upload", status, &bytes));
        }
        if root != "CompleteMultipartUploadResult" {
            bail!("unexpected CompleteMultipartUpload XML root: {root}");
        }
        Ok(())
    }

    async fn abort_upload(&self, key: &str, upload: &str) -> Result<()> {
        let query = vec![("uploadId".to_owned(), upload.to_owned())];
        let response = self
            .send_signed(Method::DELETE, key, &query, HeaderMap::new(), Bytes::new())
            .await
            .context("abort multipart upload")?;
        self.require_success("abort multipart upload", response)
            .await?;
        Ok(())
    }
}

struct CheckedReader {
    inner: Pin<Box<dyn AsyncRead + Send>>,
    expected: Option<u64>,
    read: u64,
    failed: bool,
}

impl AsyncRead for CheckedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.failed {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "S3 response length validation already failed",
            )));
        }
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let before = buffer.filled().len();
        match self.inner.as_mut().poll_read(cx, buffer) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => {
                let count = (buffer.filled().len() - before) as u64;
                self.read += count;
                if let Some(expected) = self.expected {
                    if self.read > expected {
                        self.failed = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("S3 response exceeded advertised length {expected}"),
                        )));
                    }
                    if count == 0 && self.read != expected {
                        self.failed = true;
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            format!(
                                "S3 response ended at {} bytes; expected {expected}",
                                self.read
                            ),
                        )));
                    }
                }
                Poll::Ready(Ok(()))
            }
        }
    }
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

fn put_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_bytes(name.as_bytes()).context("invalid HTTP header name")?,
        HeaderValue::from_str(value)
            .with_context(|| format!("invalid HTTP header value for {name}"))?,
    );
    Ok(())
}

fn host_header(url: &Url) -> Result<String> {
    let host = url.host_str().context("URL has no host")?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

fn canonical_headers(headers: &HeaderMap, host: &str) -> Result<(String, String)> {
    let mut values = BTreeMap::<String, String>::new();
    values.insert("host".into(), host.into());
    for (name, value) in headers {
        let value = value
            .to_str()
            .with_context(|| format!("non-text signed header: {name}"))?
            .split_ascii_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        values
            .entry(name.as_str().to_ascii_lowercase())
            .and_modify(|existing| {
                existing.push(',');
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    let canonical = values
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let signed = values.keys().cloned().collect::<Vec<_>>().join(";");
    Ok((canonical, signed))
}

fn signing_key(secret: &[u8], date: &str, region: &str, service: &str) -> Vec<u8> {
    fn hmac(key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any size");
        mac.update(value);
        mac.finalize().into_bytes().to_vec()
    }
    let mut date_key = b"AWS4".to_vec();
    date_key.extend_from_slice(secret);
    let date_key = hmac(&date_key, date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    hmac(&service_key, b"aws4_request")
}

fn aws_encode(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn encode_path_segment(input: &str) -> String {
    aws_encode(input)
}

fn encode_key(input: &str) -> String {
    input
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

async fn read_limited(mut response: Response, limit: usize) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read HTTP response")? {
        if result.len().saturating_add(chunk.len()) > limit {
            bail!("HTTP response exceeds {limit}-byte limit");
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

fn parse_s3_error(bytes: &[u8]) -> Result<S3ErrorXml> {
    from_reader(bytes).context("parse S3 error XML")
}

fn xml_root_name(bytes: &[u8]) -> Result<String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event()? {
            quick_xml::events::Event::Start(event) => {
                return Ok(String::from_utf8_lossy(event.local_name().as_ref()).into_owned())
            }
            quick_xml::events::Event::Empty(event) => {
                return Ok(String::from_utf8_lossy(event.local_name().as_ref()).into_owned())
            }
            quick_xml::events::Event::Eof => bail!("empty XML document"),
            _ => {}
        }
    }
}

fn error_from_xml(operation: &str, status: StatusCode, bytes: &[u8]) -> anyhow::Error {
    match parse_s3_error(bytes) {
        Ok(error) => {
            let definite_rejection = is_definite_rejection_status(status)
                || (status.is_success() && error.code.is_some());
            anyhow::Error::new(HttpStatusFailure {
                operation: operation.to_owned(),
                status,
                code: error.code,
                message: error.message,
                detail: None,
                definite_rejection,
            })
        }
        Err(_) => anyhow::Error::new(HttpStatusFailure {
            operation: operation.to_owned(),
            status,
            code: None,
            message: None,
            detail: Some(format!("response body: {}", String::from_utf8_lossy(bytes))),
            definite_rejection: is_definite_rejection_status(status),
        }),
    }
}

fn is_definite_rejection_status(status: StatusCode) -> bool {
    status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT
}

fn attach_probe_cleanup(error: anyhow::Error, cleanup: Result<()>) -> anyhow::Error {
    match cleanup {
        Ok(()) => error,
        Err(cleanup_error) => error.context(format!(
            "conditional-put probe cleanup also failed: {cleanup_error:#}"
        )),
    }
}

fn load_static_credentials() -> Result<Option<Credentials>> {
    let access = env::var("AWS_ACCESS_KEY_ID").ok();
    let secret = env::var("AWS_SECRET_ACCESS_KEY").ok();
    match (access, secret) {
        (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
            return Ok(Some(Credentials {
                access_key: access,
                secret_key: secret,
                session_token: env::var("AWS_SESSION_TOKEN").ok().filter(|v| !v.is_empty()),
                expires_at: None,
            }));
        }
        (None, None) => {}
        _ => bail!("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must both be set and non-empty"),
    }
    let profile = env::var("AWS_PROFILE").unwrap_or_else(|_| "default".to_owned());
    let path = env::var_os("AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".aws/credentials")));
    let Some(path) = path else {
        return Ok(None);
    };
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("open AWS credentials file {}", path.display()))
        }
    };
    let mut contents = String::new();
    file.take(MAX_XML_BYTES as u64 + 1)
        .read_to_string(&mut contents)
        .with_context(|| format!("read AWS credentials file {}", path.display()))?;
    if contents.len() > MAX_XML_BYTES {
        bail!(
            "AWS credentials file {} exceeds {MAX_XML_BYTES}-byte limit",
            path.display()
        );
    }
    parse_credentials_file(&contents, &profile)
}

fn parse_credentials_file(contents: &str, profile: &str) -> Result<Option<Credentials>> {
    let mut in_profile = false;
    let mut fields = BTreeMap::<String, String>::new();
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_profile = line[1..line.len() - 1].trim() == profile;
            continue;
        }
        if in_profile {
            if let Some((key, value)) = line.split_once('=') {
                fields.insert(key.trim().to_owned(), value.trim().to_owned());
            }
        }
    }
    let access = fields.remove("aws_access_key_id");
    let secret = fields.remove("aws_secret_access_key");
    match (access, secret) {
        (Some(access), Some(secret)) if !access.is_empty() && !secret.is_empty() => {
            Ok(Some(Credentials {
                access_key: access,
                secret_key: secret,
                session_token: fields.remove("aws_session_token"),
                expires_at: None,
            }))
        }
        (None, None) => Ok(None),
        _ => bail!("AWS credentials profile {profile:?} must contain access and secret keys"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aws_sigv4_official_iam_example_vector() {
        let key = signing_key(
            b"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20150830",
            "us-east-1",
            "iam",
        );
        let canonical_request = concat!(
            "GET\n/\nAction=ListUsers&Version=2010-05-08\n",
            "content-type:application/x-www-form-urlencoded; charset=utf-8\n",
            "host:iam.amazonaws.com\n",
            "x-amz-date:20150830T123600Z\n",
            "\ncontent-type;host;x-amz-date\n",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let scope = "20150830/us-east-1/iam/aws4_request";
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n20150830T123600Z\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(string_to_sign.as_bytes());
        assert_eq!(
            hex::encode(mac.finalize().into_bytes()),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    #[test]
    fn shared_credentials_select_profile_and_session_token() {
        let file = "[default]\naws_access_key_id=ignored\n\
                    aws_secret_access_key=ignored-secret\n\
                    [backup]\naws_access_key_id = selected\n\
                    aws_secret_access_key = selected-secret\n\
                    aws_session_token = session-token\n";
        let credentials = parse_credentials_file(file, "backup")
            .unwrap()
            .expect("selected profile exists");
        assert_eq!(credentials.access_key, "selected");
        assert_eq!(credentials.secret_key, "selected-secret");
        assert_eq!(credentials.session_token.as_deref(), Some("session-token"));
        assert!(parse_credentials_file("[partial]\naws_access_key_id=only\n", "partial").is_err());
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ImdsCredentials {
    access_key_id: String,
    secret_access_key: String,
    token: String,
    expiration: String,
}

async fn fetch_imds_credentials(client: &reqwest::Client) -> Result<Credentials> {
    let token_response = client
        .put("http://169.254.169.254/latest/api/token")
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .context("request IMDSv2 token")?
        .error_for_status()
        .context("IMDSv2 token request rejected")?;
    let token = String::from_utf8(read_limited(token_response, 16 * 1024).await?)
        .context("IMDSv2 token is not UTF-8")?;
    if token.is_empty() || token.len() > 16 * 1024 {
        bail!("IMDSv2 returned an invalid token");
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-aws-ec2-metadata-token"),
        HeaderValue::from_str(&token).context("invalid IMDSv2 token header")?,
    );
    let role_response = client
        .get("http://169.254.169.254/latest/meta-data/iam/security-credentials/")
        .headers(headers.clone())
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .context("request EC2 IAM role name")?
        .error_for_status()
        .context("EC2 IAM role lookup rejected")?;
    let role = read_limited(role_response, 16 * 1024).await?;
    let role = std::str::from_utf8(&role)
        .context("EC2 IAM role name is not UTF-8")?
        .trim();
    if role.is_empty() || role.contains(['/', '\n', '\r']) {
        bail!("EC2 metadata returned an invalid IAM role name");
    }
    let credentials_response = client
        .get(format!(
            "http://169.254.169.254/latest/meta-data/iam/security-credentials/{role}"
        ))
        .headers(headers)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .context("request EC2 IAM role credentials")?
        .error_for_status()
        .context("EC2 IAM credential lookup rejected")?;
    let body = read_limited(credentials_response, 16 * 1024).await?;
    let value: ImdsCredentials =
        serde_json::from_slice(&body).context("parse EC2 IAM credentials")?;
    let expiration = DateTime::parse_from_rfc3339(&value.expiration)
        .context("parse EC2 credential expiration")?
        .with_timezone(&Utc);
    if value.access_key_id.is_empty()
        || value.secret_access_key.is_empty()
        || value.token.is_empty()
        || expiration <= Utc::now()
    {
        bail!("EC2 metadata returned expired or incomplete credentials");
    }
    Ok(Credentials {
        access_key: value.access_key_id,
        secret_key: value.secret_access_key,
        session_token: Some(value.token),
        expires_at: Some(expiration),
    })
}
