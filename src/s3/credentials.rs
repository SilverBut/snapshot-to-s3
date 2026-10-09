//! AWS credentials from the environment or shared credentials file.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::env;
use std::io::{self, Read};
use std::path::PathBuf;

const MAX_CREDENTIALS_FILE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub(super) struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl Credentials {
    pub(super) fn from_env() -> Result<Self> {
        load_static_credentials()?.context(
            "no AWS credentials found; set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY or configure a shared credentials file",
        )
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
            }))
        }
        (None, None) => Ok(None),
        _ => bail!("AWS credentials profile {profile:?} must contain access and secret keys"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScopedEnv;

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

    #[test]
    fn shared_credentials_reject_empty_keys_and_skip_absent_profiles() {
        assert!(
            parse_credentials_file("[other]\naws_access_key_id=a\n", "backup")
                .unwrap()
                .is_none()
        );
        for file in [
            "[p]\naws_access_key_id=\naws_secret_access_key=secret\n",
            "[p]\naws_access_key_id=access\naws_secret_access_key=\n",
        ] {
            assert!(parse_credentials_file(file, "p").is_err(), "{file:?}");
        }
        // Only a whole bracketed line starts a profile.
        let file = "[p]\naws_access_key_id=a\naws_secret_access_key=s\naws_session_token=t]\n";
        let credentials = parse_credentials_file(file, "p").unwrap().unwrap();
        assert_eq!(credentials.session_token.as_deref(), Some("t]"));
    }

    const ENV: [&str; 6] = [
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "AWS_PROFILE",
        "AWS_SHARED_CREDENTIALS_FILE",
        "AWS_EC2_METADATA_DISABLED",
    ];

    /// Holds the environment lock; take only one per test.
    fn scoped_env(file: &std::path::Path) -> ScopedEnv {
        let mut env = ScopedEnv::new(&ENV.map(|name| (name, None)));
        env.set("AWS_SHARED_CREDENTIALS_FILE", file.to_str());
        env
    }

    #[test]
    fn static_credentials_come_from_environment_then_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let mut env = scoped_env(&path);

        assert!(load_static_credentials().unwrap().is_none());

        std::fs::write(
            &path,
            "[default]\naws_access_key_id=file\naws_secret_access_key=file-secret\n",
        )
        .unwrap();
        let from_file = load_static_credentials().unwrap().unwrap();
        assert_eq!(from_file.access_key, "file");

        env.set("AWS_ACCESS_KEY_ID", Some("env"));
        assert!(load_static_credentials().is_err(), "secret missing");
        env.set("AWS_SECRET_ACCESS_KEY", Some("env-secret"));
        env.set("AWS_SESSION_TOKEN", Some(""));
        let from_env = load_static_credentials().unwrap().unwrap();
        assert_eq!(from_env.access_key, "env");
        assert_eq!(from_env.secret_key, "env-secret");
        assert_eq!(from_env.session_token, None);
        env.set("AWS_ACCESS_KEY_ID", Some(""));
        assert!(load_static_credentials().is_err(), "access key empty");

        env.set("AWS_ACCESS_KEY_ID", None);
        env.set("AWS_SECRET_ACCESS_KEY", None);
        let not_a_dir = dir.path().join("credentials/nested");
        env.set("AWS_SHARED_CREDENTIALS_FILE", not_a_dir.to_str());
        let error = format!("{:#}", load_static_credentials().err().unwrap());
        assert!(error.starts_with("open AWS credentials file"), "{error}");
    }

    #[test]
    fn credentials_file_size_limit_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        let _env = scoped_env(&path);
        let entry = "[default]\naws_access_key_id=a\naws_secret_access_key=last-byte\n";
        let padding = "#".repeat(MAX_CREDENTIALS_FILE_BYTES - entry.len() - 1) + "\n";
        std::fs::write(&path, padding.clone() + entry).unwrap();
        let credentials = load_static_credentials().unwrap().unwrap();
        assert_eq!(credentials.secret_key, "last-byte");

        std::fs::write(&path, padding + "#" + entry).unwrap();
        let error = load_static_credentials().err().unwrap().to_string();
        assert!(error.contains("exceeds 1048576-byte limit"), "{error}");
    }

    #[test]
    fn provider_requires_a_credential_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = scoped_env(&dir.path().join("absent"));
        env.set("AWS_EC2_METADATA_DISABLED", Some("TRUE"));
        assert!(CredentialProvider::from_env(reqwest::Client::new()).is_err());
        env.set("AWS_ACCESS_KEY_ID", Some("a"));
        env.set("AWS_SECRET_ACCESS_KEY", Some("s"));
        assert!(CredentialProvider::from_env(reqwest::Client::new()).is_ok());
    }

    /// Serves one HTTP response with `body` and returns its URL.
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await;
            let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        });
        url
    }

    async fn read_served(body: Vec<u8>) -> Result<Vec<u8>> {
        let url = serve_once(body).await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        read_limited(client.get(url).send().await.unwrap()).await
    }

    #[tokio::test]
    async fn metadata_responses_are_bounded() {
        let full: Vec<u8> = (0..MAX_IMDS_RESPONSE_BYTES).map(|i| i as u8).collect();
        assert_eq!(read_served(full.clone()).await.unwrap(), full);
        let mut over = full;
        over.push(0);
        let error = read_served(over).await.unwrap_err().to_string();
        assert!(error.contains("exceeds 16384-byte limit"), "{error}");
    }
}
