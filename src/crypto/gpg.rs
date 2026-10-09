//! GPG recipient resolution and data-key wrapping via the `gpg` CLI.
//!
//! The user's keyring and trust policy apply. Key material only passes
//! through pipes and zeroizing buffers.

use anyhow::{bail, ensure, Context, Result};
use std::io;
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use zeroize::{Zeroize, Zeroizing};

const MAX_GPG_INPUT_SIZE: usize = 1024 * 1024;
const MAX_GPG_STDOUT_SIZE: usize = 1024 * 1024;
const MAX_GPG_STDERR_SIZE: usize = 16 * 1024;

/// Resolves a selector to exactly one full fingerprint with an encryption-capable key.
pub async fn resolve_recipient(selector: &str) -> Result<String> {
    select_unique_fingerprint(&list_recipient_keys(selector).await?, true)
}

/// Resolves a selector to one primary fingerprint for decrypting historical backups.
///
/// Unlike [`resolve_recipient`], this lookup does not require the public key to be
/// currently encryption-capable. Expired, revoked, or encryption-ineligible keys may
/// still identify the matching historical secret key in the user's GPG keyring.
pub async fn resolve_decryption_recipient(selector: &str) -> Result<String> {
    select_unique_fingerprint(&list_recipient_keys(selector).await?, false)
}

/// Encrypts a 32-byte data key to a public-key recipient.
pub async fn encrypt_key(fingerprint: &str, key: &[u8; 32]) -> Result<Vec<u8>> {
    ensure!(
        is_full_fingerprint(fingerprint),
        "GPG recipient must be a full 40- or 64-character fingerprint"
    );
    let fingerprint = fingerprint.to_ascii_uppercase();
    let output = run_gpg(
        &["--encrypt", "--recipient", &fingerprint, "--output", "-"],
        Some(key),
        MAX_GPG_STDOUT_SIZE,
    )
    .await
    .context("GPG public-key encryption failed")?;
    ensure!(!output.is_empty(), "GPG produced empty encrypted key");
    Ok(output.to_vec())
}

/// Decrypts a wrapped data key without placing plaintext on disk.
pub async fn decrypt_key(ciphertext: &[u8]) -> Result<Zeroizing<[u8; 32]>> {
    ensure!(
        !ciphertext.is_empty() && ciphertext.len() <= MAX_GPG_INPUT_SIZE,
        "GPG encrypted key has an invalid size"
    );
    let mut output = run_gpg(&["--decrypt"], Some(ciphertext), 33)
        .await
        .context("GPG private-key decryption failed")?;
    ensure!(
        output.len() == 32,
        "decrypted GPG key must contain exactly 32 bytes"
    );
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&output);
    output.zeroize();
    Ok(key)
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ListedRecipient {
    fingerprint: String,
    encryption_capable: bool,
}

async fn list_recipient_keys(selector: &str) -> Result<Vec<ListedRecipient>> {
    ensure!(
        !selector.is_empty()
            && !selector.starts_with('-')
            && !selector.contains(['\0', '\n', '\r']),
        "invalid GPG recipient selector"
    );
    let output = run_gpg(
        &[
            "--list-keys",
            "--with-colons",
            "--fixed-list-mode",
            "--with-subkey-fingerprint",
            "--",
            selector,
        ],
        None,
        MAX_GPG_STDOUT_SIZE,
    )
    .await
    .context("list GPG recipient keys")?;
    let text = std::str::from_utf8(&output).context("GPG key listing is not UTF-8")?;
    Ok(parse_gpg_key_listing(text))
}

/// Primary key state while scanning its `pub`, `fpr` and `sub` records.
#[derive(Default)]
struct PrimaryKey {
    fingerprint: Option<String>,
    valid: bool,
    encryption_capable: bool,
    awaiting_fingerprint: bool,
}

/// Parses `--with-colons` output into unique primary keys. A key is
/// encryption-capable if it is valid and it or a valid subkey can encrypt.
fn parse_gpg_key_listing(text: &str) -> Vec<ListedRecipient> {
    let mut recipients = Vec::new();
    let mut key = PrimaryKey::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields[0] {
            "pub" => {
                push_unique(&mut recipients, std::mem::take(&mut key));
                let valid = record_is_valid(&fields);
                key = PrimaryKey {
                    fingerprint: None,
                    valid,
                    encryption_capable: valid && can_encrypt(&fields),
                    awaiting_fingerprint: true,
                };
            }
            "sub" => {
                key.encryption_capable |=
                    key.valid && record_is_valid(&fields) && can_encrypt(&fields);
                key.awaiting_fingerprint = false;
            }
            "fpr" if key.awaiting_fingerprint => {
                if let Some(fingerprint) = fields.get(9) {
                    key.fingerprint = Some(fingerprint.to_ascii_uppercase());
                }
                key.awaiting_fingerprint = false;
            }
            _ => {}
        }
    }
    push_unique(&mut recipients, key);
    recipients
}

fn push_unique(recipients: &mut Vec<ListedRecipient>, key: PrimaryKey) {
    if let Some(fingerprint) = key.fingerprint {
        if !recipients.iter().any(|r| r.fingerprint == fingerprint) {
            recipients.push(ListedRecipient {
                fingerprint,
                encryption_capable: key.encryption_capable,
            });
        }
    }
}

/// Not expired, revoked, disabled or invalid.
fn record_is_valid(fields: &[&str]) -> bool {
    !fields.get(1).is_some_and(|validity| {
        ["e", "r", "d", "i"]
            .iter()
            .any(|invalid| validity.eq_ignore_ascii_case(invalid))
    })
}

fn can_encrypt(fields: &[&str]) -> bool {
    fields
        .get(11)
        .is_some_and(|caps| caps.to_ascii_lowercase().contains('e'))
}

fn is_full_fingerprint(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn select_unique_fingerprint(
    recipients: &[ListedRecipient],
    require_encryption: bool,
) -> Result<String> {
    let mut matches = recipients
        .iter()
        .filter(|recipient| !require_encryption || recipient.encryption_capable);
    let recipient = matches
        .next()
        .context("GPG selector did not match a suitable primary key")?;
    ensure!(
        matches.next().is_none(),
        "GPG selector must resolve to exactly one suitable primary key"
    );
    ensure!(
        is_full_fingerprint(&recipient.fingerprint),
        "GPG returned an invalid primary fingerprint"
    );
    Ok(recipient.fingerprint.clone())
}

/// Runs `gpg --batch --no-tty` with bounded, zeroizing stdout capture.
async fn run_gpg(
    arguments: &[&str],
    input: Option<&[u8]>,
    stdout_limit: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    if let Some(input) = input {
        ensure!(
            input.len() <= MAX_GPG_INPUT_SIZE,
            "GPG input exceeds the configured size limit"
        );
    }
    let mut child = Command::new("gpg")
        .args(["--batch", "--no-tty"])
        .args(arguments)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn gpg")?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped GPG stdout");
    let stderr = child.stderr.take().expect("piped GPG stderr");
    let input_task = async move {
        match (stdin, input) {
            (Some(mut stdin), Some(bytes)) => stdin.write_all(bytes).await.context("write GPG input"),
            (None, Some(_)) => bail!("GPG stdin pipe was not created"),
            (Some(mut stdin), None) => stdin.shutdown().await.context("close GPG stdin"),
            (None, None) => Ok(()),
        }
    };
    let (stdout_result, stderr_result, input_result, status_result) = tokio::join!(
        collect_limited(stdout, stdout_limit),
        collect_limited(stderr, MAX_GPG_STDERR_SIZE),
        input_task,
        child.wait()
    );
    let (stdout, stdout_overflow) = stdout_result?;
    let (stderr, _) = stderr_result?;
    input_result?;
    let status = status_result.context("wait for gpg")?;
    ensure!(
        !stdout_overflow,
        "GPG stdout exceeded the configured size limit"
    );
    if !status.success() {
        let diagnostic = String::from_utf8_lossy(&stderr);
        bail!("gpg exited with {status}: {}", diagnostic.trim());
    }
    Ok(stdout)
}

/// Reads to EOF, keeping at most `limit` bytes; the flag reports overflow.
async fn collect_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> io::Result<(Zeroizing<Vec<u8>>, bool)> {
    let mut collected = Zeroizing::new(Vec::with_capacity(limit.min(8192)));
    let mut overflow = false;
    let mut buffer = Zeroizing::new([0u8; 8192]);
    loop {
        let count = reader.read(&mut buffer[..]).await?;
        if count == 0 {
            break;
        }
        let keep = limit.saturating_sub(collected.len()).min(count);
        collected.extend_from_slice(&buffer[..keep]);
        overflow |= keep != count;
    }
    Ok((collected, overflow))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn historical_recipient_selection_ignores_expiry_and_encryption_capability() {
        let fixtures = [
            (
                include_str!("../../tests/fixtures/gpg_expired_key.colons"),
                "A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1",
            ),
            (
                include_str!("../../tests/fixtures/gpg_no_encrypt_key.colons"),
                "B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2B2",
            ),
        ];
        for (fixture, expected_fingerprint) in fixtures {
            let listed = parse_gpg_key_listing(fixture);
            assert_eq!(listed.len(), 1);
            assert!(!listed[0].encryption_capable);
            assert_eq!(
                select_unique_fingerprint(&listed, false).unwrap(),
                expected_fingerprint
            );
            assert!(select_unique_fingerprint(&listed, true).is_err());
        }
    }
}
