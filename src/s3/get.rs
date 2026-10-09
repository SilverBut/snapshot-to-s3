//! Resumable, ETag-pinned GET with exact length accounting.

use super::throughput::ThroughputGuard;
use super::{is_retryable, put_header, HttpStore};
use crate::model::Reader;
use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use reqwest::header::{HeaderMap, CONTENT_LENGTH, ETAG};
use reqwest::{Method, Response, StatusCode};
use std::io;
use tokio_util::io::StreamReader;

impl HttpStore {
    /// Opens `key` and returns a reader that resumes interrupted bodies with
    /// range requests pinned to the first response's strong ETag.
    pub(super) async fn get_object(
        &self,
        key: &str,
        etag: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> Result<Reader> {
        if let Some((start, end)) = range {
            if start > end {
                bail!("invalid byte range: start exceeds end");
            }
            (end - start)
                .checked_add(1)
                .context("GET range length overflow")?;
        }
        let mut retries = 0;
        let response = loop {
            match self.get_response(key, etag, range).await {
                Ok(response) => break response,
                Err(error) if is_retryable(&error) && retries < self.policy.get_retries => {
                    tokio::time::sleep(self.policy.retry_delay(retries)).await;
                    retries += 1;
                }
                Err(error) => return Err(error),
            }
        };
        let identity = GetIdentity::validate(&response, range, etag, None)?;
        let state = GetStream {
            store: self.clone(),
            key: key.to_owned(),
            response: Some(response),
            identity,
            consumed: 0,
            retries,
            failed: false,
            guard: ThroughputGuard::new(&self.policy),
        };
        let stream = futures_util::stream::unfold(state, |mut state| async move {
            let item = state.next_chunk().await?;
            Some((item, state))
        });
        Ok(Box::new(StreamReader::new(Box::pin(stream))))
    }

    async fn get_response(
        &self,
        key: &str,
        etag: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> Result<Response> {
        let mut headers = HeaderMap::new();
        // Ciphertext offsets must refer to the wire representation, never decoded bytes.
        put_header(&mut headers, "accept-encoding", "identity")?;
        if let Some(etag) = etag {
            put_header(&mut headers, "if-match", etag)?;
        }
        if let Some((start, end)) = range {
            put_header(&mut headers, "range", &format!("bytes={start}-{end}"))?;
        }
        let response = self
            .send_signed(Method::GET, key, &[], headers, Bytes::new())
            .await?;
        let expected_status = if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        };
        if response.status() != expected_status {
            return Err(self.response_error("GET object", response).await);
        }
        Ok(response)
    }
}

/// What the first response promised; resumed responses must agree.
struct GetIdentity {
    etag: String,
    start: u64,
    end: u64,
    length: u64,
    total: u64,
}

impl GetIdentity {
    fn validate(
        response: &Response,
        range: Option<(u64, u64)>,
        pin: Option<&str>,
        total: Option<u64>,
    ) -> Result<Self> {
        let headers = response.headers();
        if headers
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
        {
            bail!("GET returned non-identity Content-Encoding");
        }
        let etag = headers
            .get(ETAG)
            .context("GET response omitted ETag")?
            .to_str()?
            .to_owned();
        if !etag.starts_with('"') || !etag.ends_with('"') || etag.len() < 2 {
            bail!("GET requires a strong ETag");
        }
        if pin.is_some_and(|pin| pin != etag) {
            bail!("GET ETag mismatch");
        }
        let length = headers
            .get(CONTENT_LENGTH)
            .context("GET response omitted Content-Length")?
            .to_str()?
            .parse::<u64>()?;
        let (start, end, actual_total) = if let Some((start, end)) = range {
            let value = headers
                .get("content-range")
                .context("range GET omitted Content-Range")?
                .to_str()?;
            let actual_total = value
                .strip_prefix(&format!("bytes {start}-{end}/"))
                .and_then(|value| value.parse::<u64>().ok())
                .context("range GET returned malformed or mismatched Content-Range")?;
            let expected = end
                .checked_sub(start)
                .and_then(|v| v.checked_add(1))
                .context("GET range length overflow")?;
            if actual_total <= end || length != expected {
                bail!("range GET Content-Range/Content-Length mismatch");
            }
            (start, end, actual_total)
        } else {
            if headers.contains_key("content-range") {
                bail!("full GET unexpectedly returned Content-Range");
            }
            (0, length.saturating_sub(1), length)
        };
        if total.is_some_and(|total| total != actual_total) {
            bail!("GET resumed object length changed");
        }
        Ok(Self {
            etag,
            start,
            end,
            length,
            total: actual_total,
        })
    }
}

struct GetStream {
    store: HttpStore,
    key: String,
    response: Option<Response>,
    identity: GetIdentity,
    consumed: u64,
    retries: usize,
    failed: bool,
    guard: ThroughputGuard,
}

impl GetStream {
    /// The next body chunk; `None` after the advertised length or an error.
    async fn next_chunk(&mut self) -> Option<io::Result<Bytes>> {
        if self.failed || self.consumed == self.identity.length {
            return None;
        }
        let result = self.read_chunk().await;
        self.failed = result.is_err();
        Some(result)
    }

    async fn read_chunk(&mut self) -> io::Result<Bytes> {
        loop {
            let response = self.response.as_mut().expect("active GET response");
            let error = match self.guard.wait(response.chunk(), None).await {
                Ok(Ok(Some(chunk))) => {
                    if chunk.len() as u64 > self.identity.length - self.consumed {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "GET exceeded expected length",
                        ));
                    }
                    self.consumed += chunk.len() as u64;
                    self.guard.add(chunk.len() as u64);
                    return Ok(chunk);
                }
                Ok(Ok(None)) => anyhow!(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "GET ended before advertised length"
                )),
                Ok(Err(error)) => anyhow!(error),
                Err(error) => error,
            };
            self.response = None;
            if self.retries >= self.store.policy.get_retries {
                return Err(io::Error::other(format!(
                    "GET retries exhausted: {error:#}"
                )));
            }
            self.resume().await?;
        }
    }

    /// Requests the unread remainder, retrying transient failures.
    async fn resume(&mut self) -> io::Result<()> {
        loop {
            tokio::time::sleep(self.store.policy.retry_delay(self.retries)).await;
            self.retries += 1;
            let range = Some((self.identity.start + self.consumed, self.identity.end));
            let etag = Some(self.identity.etag.as_str());
            match self.store.get_response(&self.key, etag, range).await {
                Ok(response) => {
                    GetIdentity::validate(&response, range, etag, Some(self.identity.total))
                        .map_err(|error| {
                            io::Error::new(io::ErrorKind::InvalidData, error.to_string())
                        })?;
                    self.response = Some(response);
                    self.guard = ThroughputGuard::new(&self.store.policy);
                    return Ok(());
                }
                Err(error)
                    if is_retryable(&error) && self.retries < self.store.policy.get_retries => {}
                Err(error) => {
                    return Err(io::Error::other(format!("GET resume failed: {error:#}")))
                }
            }
        }
    }
}
