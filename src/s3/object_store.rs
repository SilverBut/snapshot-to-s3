//! [`ObjectStore`] operations over the S3 REST API.

use super::error::{is_file_already_exists, xml_root_name, HttpStatusFailure};
use super::{is_definite_rejection, is_retryable, HttpStore, LockDetectionMode};
use crate::model::{MetadataMap, Reader};
use crate::store::{ObjectHead, ObjectStore, Part};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use quick_xml::de::from_reader;
use reqwest::header::{HeaderMap, CONTENT_LENGTH, ETAG};
use reqwest::{Method, StatusCode};
use serde::Deserialize;

/// S3 part numbers are 1..=10000.
const MAX_PART_NUMBER: u32 = 10_000;

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
            .get(CONTENT_LENGTH)
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
        self.get_object(key, etag, range).await
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
        let headers = self.conditional_put_headers()?;
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
        if !response.status().is_success() {
            let error = self
                .response_error("conditional PUT object", response)
                .await;
            if self.lock_detection_mode == LockDetectionMode::XCosForbidOverwrite
                && is_file_already_exists(&error)
            {
                return Ok(false);
            }
            return Err(error);
        }
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
        let query = [("uploads".to_owned(), String::new())];
        let headers = self.metadata_headers(metadata)?;
        let response = self
            .send_signed(Method::POST, key, &query, headers, Bytes::new())
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
        if !(1..=MAX_PART_NUMBER).contains(&number) {
            bail!("multipart part number must be between 1 and 10000");
        }
        let query = [
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
                .any(|part| !(1..=MAX_PART_NUMBER).contains(&part.number))
            || parts
                .windows(2)
                .any(|window| window[0].number >= window[1].number)
        {
            bail!("multipart completion requires ordered unique part numbers in 1..=10000");
        }
        let operation = "complete multipart upload";
        let query = [("uploadId".to_owned(), upload.to_owned())];
        let body = Bytes::from(complete_upload_xml(parts));
        let response = self
            .send_signed(Method::POST, key, &query, HeaderMap::new(), body)
            .await
            .context(operation)?;
        let status = response.status();
        let bytes = self
            .read_body(response)
            .await
            .context("read CompleteMultipartUpload response")?;
        // S3 may report a failed completion as an <Error> document inside HTTP 200.
        let root = if status.is_success() {
            xml_root_name(&bytes).context("parse CompleteMultipartUpload response")?
        } else {
            "Error".into()
        };
        match root.as_str() {
            "CompleteMultipartUploadResult" => Ok(()),
            "Error" => {
                Err(HttpStatusFailure::from_error_document(operation, status, &bytes).into())
            }
            root => bail!("unexpected CompleteMultipartUpload XML root: {root}"),
        }
    }

    async fn abort_upload(&self, key: &str, upload: &str) -> Result<()> {
        let query = [("uploadId".to_owned(), upload.to_owned())];
        let response = self
            .send_signed(Method::DELETE, key, &query, HeaderMap::new(), Bytes::new())
            .await
            .context("abort multipart upload")?;
        self.require_success("abort multipart upload", response)
            .await?;
        Ok(())
    }

    fn is_retryable(&self, error: &anyhow::Error) -> bool {
        is_retryable(error)
    }

    fn is_definite_rejection(&self, error: &anyhow::Error) -> bool {
        is_definite_rejection(error)
    }
}

fn complete_upload_xml(parts: &[Part]) -> String {
    let mut xml = String::from("<CompleteMultipartUpload>");
    for part in parts {
        xml.push_str("<Part><PartNumber>");
        xml.push_str(&part.number.to_string());
        xml.push_str("</PartNumber><ETag>");
        xml.push_str(&xml_escape(&part.etag));
        xml.push_str("</ETag></Part>");
    }
    xml.push_str("</CompleteMultipartUpload>");
    xml
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
