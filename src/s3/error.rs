//! S3 error responses and their retry/rejection classification.

use anyhow::{bail, Context, Result};
use quick_xml::de::from_reader;
use reqwest::StatusCode;
use serde::Deserialize;
use std::fmt;
use std::io;

#[derive(Debug, Deserialize)]
struct S3ErrorXml {
    #[serde(rename = "Code")]
    code: Option<String>,
    #[serde(rename = "Message")]
    message: Option<String>,
}

/// A non-success response, or an `<Error>` document inside a 200 response.
#[derive(Debug)]
pub(super) struct HttpStatusFailure {
    pub operation: String,
    pub status: StatusCode,
    pub code: Option<String>,
    pub message: Option<String>,
    pub detail: Option<String>,
    pub definite_rejection: bool,
}

impl HttpStatusFailure {
    /// Builds the error from a response body; `header_code` is
    /// `x-amz-error-code`, which HEAD responses use instead of a body.
    pub fn from_body(
        operation: &str,
        status: StatusCode,
        header_code: Option<String>,
        body: &[u8],
    ) -> Self {
        let parsed = parse_s3_error(body).ok();
        let code = header_code.or_else(|| parsed.as_ref().and_then(|error| error.code.clone()));
        let message = parsed.and_then(|error| error.message);
        let detail = (code.is_none() && message.is_none() && !body.is_empty())
            .then(|| format!("response body: {}", String::from_utf8_lossy(body)));
        Self {
            operation: operation.to_owned(),
            status,
            code,
            message,
            detail,
            definite_rejection: is_definite_rejection_status(status),
        }
    }

    /// An error document, which in a 200 response also proves rejection.
    pub fn from_error_document(operation: &str, status: StatusCode, body: &[u8]) -> Self {
        let mut failure = Self::from_body(operation, status, None, body);
        if status.is_success() && failure.code.is_some() {
            failure.definite_rejection = true;
        }
        failure
    }
}

impl fmt::Display for HttpStatusFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            if error.is_timeout() || error.is_connect() || error.is_request() || error.is_body() {
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
                    | io::ErrorKind::UnexpectedEof
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

fn is_definite_rejection_status(status: StatusCode) -> bool {
    status.is_client_error() && status != StatusCode::REQUEST_TIMEOUT
}

fn parse_s3_error(bytes: &[u8]) -> Result<S3ErrorXml> {
    from_reader(bytes).context("parse S3 error XML")
}

/// Local name of the first XML element.
pub(super) fn xml_root_name(bytes: &[u8]) -> Result<String> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event()? {
            Event::Start(event) | Event::Empty(event) => {
                return Ok(String::from_utf8_lossy(event.local_name().as_ref()).into_owned())
            }
            Event::Eof => bail!("empty XML document"),
            _ => {}
        }
    }
}
