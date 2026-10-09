//! AWS credentials: environment, shared credentials file, then EC2 IMDSv2.

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Response;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_CREDENTIALS_FILE_BYTES: usize = 1024 * 1024;
const MAX_IMDS_RESPONSE_BYTES: usize = 16 * 1024;
const IMDS: &str = "http://169.254.169.254/latest";
const IMDS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
    expires_at: Option<DateTime<Utc>>,
}

pub(super) struct CredentialProvider {
    static_credentials: Option<Credentials>,
    cache: Mutex<Option<Credentials>>,
    client: reqwest::Client,
    metadata_disabled: bool,
}

impl CredentialProvider {
    pub(super) fn from_env(client: reqwest::Client) -> Result<Self> {
        let static_credentials = load_static_credentials()?;
        let metadata_disabled = env::var("AWS_EC2_METADATA_DISABLED")
            .map(|value| value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if static_credentials.is_none() && metadata_disabled {
            bail!("no AWS credentials found and EC2 metadata credentials are disabled");
        }
        Ok(Self {
            static_credentials,
            cache: Mutex::new(None),
            client,
            metadata_disabled,
        })
    }

    /// Static credentials, or cached IMDS credentials refreshed five
    /// minutes before expiry.
    pub(super) async fn get(&self) -> Result<Credentials> {
        if let Some(credentials) = &self.static_credentials {
            return Ok(credentials.clone());
        }
        let mut cached = self.cache.lock().await;
        if let Some(credentials) = cached.as_ref() {
            if credentials
                .expires_at
                .is_some_and(|expiration| expiration > Utc::now() + chrono::Duration::minutes(5))
            {
                return Ok(credentials.clone());
            }
        }
        if self.metadata_disabled {
            bail!("no AWS credentials found and EC2 metadata credentials are disabled");
        }
        let credentials = fetch_imds_credentials(&self.client).await?;
        *cached = Some(credentials.clone());
        Ok(credentials)
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
    file.take(MAX_CREDENTIALS_FILE_BYTES as u64 + 1)
        .read_to_string(&mut contents)
        .with_context(|| format!("read AWS credentials file {}", path.display()))?;
    if contents.len() > MAX_CREDENTIALS_FILE_BYTES {
        bail!(
            "AWS credentials file {} exceeds {MAX_CREDENTIALS_FILE_BYTES}-byte limit",
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
        .put(format!("{IMDS}/api/token"))
        .header("x-aws-ec2-metadata-token-ttl-seconds", "21600")
        .timeout(IMDS_TIMEOUT)
        .send()
        .await
        .context("request IMDSv2 token")?
        .error_for_status()
        .context("IMDSv2 token request rejected")?;
    let token = String::from_utf8(read_limited(token_response).await?)
        .context("IMDSv2 token is not UTF-8")?;
    if token.is_empty() {
        bail!("IMDSv2 returned an invalid token");
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("x-aws-ec2-metadata-token"),
        HeaderValue::from_str(&token).context("invalid IMDSv2 token header")?,
    );
    let roles = format!("{IMDS}/meta-data/iam/security-credentials/");
    let role_response = client
        .get(&roles)
        .headers(headers.clone())
        .timeout(IMDS_TIMEOUT)
        .send()
        .await
        .context("request EC2 IAM role name")?
        .error_for_status()
        .context("EC2 IAM role lookup rejected")?;
    let role = read_limited(role_response).await?;
    let role = std::str::from_utf8(&role)
        .context("EC2 IAM role name is not UTF-8")?
        .trim();
    if role.is_empty() || role.contains(['/', '\n', '\r']) {
        bail!("EC2 metadata returned an invalid IAM role name");
    }
    let credentials_response = client
        .get(format!("{roles}{role}"))
        .headers(headers)
        .timeout(IMDS_TIMEOUT)
        .send()
        .await
        .context("request EC2 IAM role credentials")?
        .error_for_status()
        .context("EC2 IAM credential lookup rejected")?;
    let body = read_limited(credentials_response).await?;
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

async fn read_limited(mut response: Response) -> Result<Vec<u8>> {
    let mut result = Vec::new();
    while let Some(chunk) = response.chunk().await.context("read HTTP response")? {
        if result.len().saturating_add(chunk.len()) > MAX_IMDS_RESPONSE_BYTES {
            bail!("HTTP response exceeds {MAX_IMDS_RESPONSE_BYTES}-byte limit");
        }
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

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
