//! AWS Signature Version 4 header signing and URI encoding.

use super::credentials::Credentials;
use super::put_header;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::header::HeaderMap;
use reqwest::{Method, Url};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Region and service of the credential scope.
pub(super) struct Scope<'a> {
    pub region: &'a str,
    pub service: &'a str,
}

/// Adds the date, payload hash, session token and `authorization` headers.
/// The host is signed but left for the HTTP client to send.
pub(super) fn sign(
    headers: &mut HeaderMap,
    method: &Method,
    url: &Url,
    body: &[u8],
    credentials: &Credentials,
    scope: &Scope<'_>,
    now: DateTime<Utc>,
) -> Result<()> {
    let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date = now.format("%Y%m%d").to_string();
    let payload_hash = hex::encode(Sha256::digest(body));
    put_header(headers, "x-amz-date", &amz_date)?;
    put_header(headers, "x-amz-content-sha256", &payload_hash)?;
    if let Some(token) = &credentials.session_token {
        put_header(headers, "x-amz-security-token", token)?;
    }
    let (canonical_headers, signed_headers) = canonical_headers(headers, &host_header(url)?)?;
    let canonical_request = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
        method.as_str(),
        url.path(),
        url.query().unwrap_or(""),
    );
    let credential_scope = format!("{date}/{}/{}/aws4_request", scope.region, scope.service);
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let key = signing_key(
        credentials.secret_key.as_bytes(),
        &date,
        scope.region,
        scope.service,
    );
    let signature = hex::encode(hmac(&key, string_to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, \
         SignedHeaders={signed_headers}, Signature={signature}",
        credentials.access_key
    );
    put_header(headers, "authorization", &authorization)
}

/// SigV4 URI encoding: everything except unreserved characters.
pub(super) fn uri_encode(input: &str) -> String {
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

/// Encodes an object key as path segments, keeping `/`.
pub(super) fn encode_key(input: &str) -> String {
    input
        .split('/')
        .map(uri_encode)
        .collect::<Vec<_>>()
        .join("/")
}

fn hmac(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any size");
    mac.update(value);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &[u8], date: &str, region: &str, service: &str) -> Vec<u8> {
    let mut date_key = b"AWS4".to_vec();
    date_key.extend_from_slice(secret);
    let date_key = hmac(&date_key, date.as_bytes());
    let region_key = hmac(&date_key, region.as_bytes());
    let service_key = hmac(&region_key, service.as_bytes());
    hmac(&service_key, b"aws4_request")
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

/// Canonical header block and signed-header list, including `host`.
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
        assert_eq!(
            hex::encode(hmac(&key, string_to_sign.as_bytes())),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }
}
