use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes128Gcm, Nonce,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hkdf::Hkdf;
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
};
use zeroize::{Zeroize, Zeroizing};

const HEADER_SIZE: usize = 24;
const SALT_SIZE: usize = 16;
const NONCE_PREFIX_SIZE: usize = 7;
const SALT_OFFSET: usize = 1;
const NONCE_PREFIX_OFFSET: usize = SALT_OFFSET + SALT_SIZE;
const TAG_SIZE: usize = 16;
const CIPHERTEXT_SEGMENT_SIZE: usize = 1_048_576;
const FIRST_PLAINTEXT_SEGMENT_SIZE: usize = CIPHERTEXT_SEGMENT_SIZE - HEADER_SIZE - TAG_SIZE;
const PLAINTEXT_SEGMENT_SIZE: usize = CIPHERTEXT_SEGMENT_SIZE - TAG_SIZE;
const MAX_GPG_INPUT_SIZE: usize = 1024 * 1024;
const MAX_GPG_STDOUT_SIZE: usize = 1024 * 1024;
const MAX_GPG_STDERR_SIZE: usize = 16 * 1024;

/// Generate a cryptographically random, zeroizing 256-bit backup key.
pub fn generate_key() -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *key);
    key
}

/// Return the required lowercase SHA-256 checksum representation.
pub fn checksum(key: &[u8; 32]) -> Vec<u8> {
    let digest = Sha256::digest(key);
    format!("{}\n", hex::encode(digest)).into_bytes()
}

/// Verify the exact 64-lowercase-hex-character-plus-LF checksum.
pub fn verify_checksum(key: &[u8; 32], supplied: &[u8]) -> Result<()> {
    ensure!(
        supplied == checksum(key),
        "key SHA-256 checksum does not match"
    );
    Ok(())
}

/// Encrypt an arbitrarily long stream using Tink's AES128_GCM_HKDF_1MB framing.
pub async fn encrypt<R, W>(key: &[u8; 32], aad: &[u8], input: &mut R, output: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut header = [0u8; HEADER_SIZE];
    OsRng.fill_bytes(&mut header);
    header[0] = HEADER_SIZE as u8;
    encrypt_with_header(key, aad, input, output, &header).await
}

/// Deterministic header entry point used by conformance tests.
pub(crate) async fn encrypt_with_header<R, W>(
    key: &[u8; 32],
    aad: &[u8],
    input: &mut R,
    output: &mut W,
    header: &[u8; HEADER_SIZE],
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    ensure!(
        header[0] as usize == HEADER_SIZE,
        "invalid Tink streaming header length"
    );
    output
        .write_all(header)
        .await
        .context("write encryption header")?;
    let aead_key = derive_key(key, &header[SALT_OFFSET..NONCE_PREFIX_OFFSET], aad)?;
    let cipher = Aes128Gcm::new_from_slice(&aead_key[..]).expect("fixed AES-128 key size");
    let nonce_prefix = &header[NONCE_PREFIX_OFFSET..];
    let mut index = 0u32;
    let mut carry = None;
    let mut first = true;

    loop {
        let capacity = if first {
            FIRST_PLAINTEXT_SEGMENT_SIZE
        } else {
            PLAINTEXT_SEGMENT_SIZE
        };
        let mut plaintext = Zeroizing::new(Vec::with_capacity(capacity));
        if let Some(byte) = carry.take() {
            plaintext.push(byte);
        }

        let mut eof = false;
        while plaintext.len() < capacity {
            let mut chunk = [0u8; 8192];
            let remaining = capacity - plaintext.len();
            let read_limit = remaining.min(chunk.len());
            let count = input
                .read(&mut chunk[..read_limit])
                .await
                .context("read plaintext stream")?;
            if count == 0 {
                eof = true;
                break;
            }
            plaintext.extend_from_slice(&chunk[..count]);
            chunk[..count].zeroize();
        }

        if !eof && plaintext.len() == capacity {
            let mut lookahead = [0u8; 1];
            if input
                .read(&mut lookahead)
                .await
                .context("read plaintext segment boundary")?
                == 0
            {
                eof = true;
            } else {
                carry = Some(lookahead[0]);
                lookahead.zeroize();
            }
        }

        let final_segment = eof;
        if !final_segment && index == u32::MAX {
            bail!("AES-GCM-HKDF stream exceeds the segment counter limit");
        }
        let nonce = make_nonce(nonce_prefix, index, final_segment);
        let nonce = Nonce::from(nonce);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: &[],
                },
            )
            .map_err(|_| anyhow!("encrypt AES-GCM-HKDF segment"))?;
        output
            .write_all(&ciphertext)
            .await
            .context("write encrypted segment")?;
        plaintext.zeroize();

        if final_segment {
            break;
        }
        first = false;
        index = index
            .checked_add(1)
            .context("AES-GCM-HKDF segment counter overflow")?;
    }

    output.flush().await.context("flush encrypted stream")
}

/// Authenticate and decrypt every segment, including the final segment.
pub async fn decrypt<R, W>(key: &[u8; 32], aad: &[u8], input: &mut R, output: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut header = [0u8; HEADER_SIZE];
    input
        .read_exact(&mut header)
        .await
        .context("truncated AES-GCM-HKDF header")?;
    ensure!(
        header[0] as usize == HEADER_SIZE,
        "invalid Tink streaming header length"
    );
    let aead_key = derive_key(key, &header[SALT_OFFSET..NONCE_PREFIX_OFFSET], aad)?;
    let cipher = Aes128Gcm::new_from_slice(&aead_key[..]).expect("fixed AES-128 key size");
    let nonce_prefix = &header[NONCE_PREFIX_OFFSET..];
    let mut index = 0u32;
    let mut carry = None;
    let mut first = true;

    loop {
        let maximum = if first {
            FIRST_PLAINTEXT_SEGMENT_SIZE + TAG_SIZE
        } else {
            PLAINTEXT_SEGMENT_SIZE + TAG_SIZE
        };
        let mut segment = Vec::with_capacity(maximum);
        if let Some(byte) = carry.take() {
            segment.push(byte);
        }
        while segment.len() < maximum {
            let mut chunk = [0u8; 8192];
            let limit = (maximum - segment.len()).min(chunk.len());
            let count = input
                .read(&mut chunk[..limit])
                .await
                .context("read encrypted segment")?;
            if count == 0 {
                break;
            }
            segment.extend_from_slice(&chunk[..count]);
        }

        if segment.is_empty() {
            bail!("encrypted stream has no authenticated final segment");
        }
        let mut final_segment = segment.len() < maximum;
        if !final_segment {
            let mut lookahead = [0u8; 1];
            if input
                .read(&mut lookahead)
                .await
                .context("read encrypted segment boundary")?
                == 0
            {
                final_segment = true;
            } else {
                carry = Some(lookahead[0]);
                lookahead.zeroize();
            }
        }

        ensure!(
            segment.len() >= TAG_SIZE,
            "truncated AES-GCM-HKDF segment tag"
        );
        let nonce = make_nonce(nonce_prefix, index, final_segment);
        let nonce = Nonce::from(nonce);
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &segment,
                        aad: &[],
                    },
                )
                .map_err(|_| anyhow!("authenticate AES-GCM-HKDF segment {index}"))?,
        );
        output
            .write_all(&plaintext)
            .await
            .context("write authenticated plaintext segment")?;

        if final_segment {
            break;
        }
        first = false;
        index = index
            .checked_add(1)
            .context("AES-GCM-HKDF segment counter overflow")?;
    }
    output.flush().await.context("flush decrypted stream")
}

/// Authenticate the complete initial segments needed for restore preflight.
///
/// `input` must expose the object prefix of `min(total_ciphertext_bytes, 2 MiB)` bytes,
/// such as an HTTP Range response. The known object size determines whether the last
/// inspected segment is final; no EOF is required for a non-final prefix.
pub async fn verify_prefix<R>(
    key: &[u8; 32],
    aad: &[u8],
    input: &mut R,
    total_ciphertext_bytes: u64,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
{
    ensure!(
        total_ciphertext_bytes >= (HEADER_SIZE + TAG_SIZE) as u64,
        "encrypted object is too short to contain an authenticated segment"
    );
    let mut header = [0u8; HEADER_SIZE];
    input
        .read_exact(&mut header)
        .await
        .context("truncated AES-GCM-HKDF header in prefix")?;
    ensure!(
        header[0] as usize == HEADER_SIZE,
        "invalid Tink streaming header length"
    );
    let aead_key = derive_key(key, &header[SALT_OFFSET..NONCE_PREFIX_OFFSET], aad)?;
    let cipher = Aes128Gcm::new_from_slice(&aead_key[..]).expect("fixed AES-128 key size");
    let nonce_prefix = &header[NONCE_PREFIX_OFFSET..];
    let mut remaining = total_ciphertext_bytes - HEADER_SIZE as u64;
    let mut index = 0u32;
    let mut first = true;
    let mut processed = 0u64;
    let prefix_limit = (CIPHERTEXT_SEGMENT_SIZE * 2 - HEADER_SIZE) as u64;

    while processed < prefix_limit && remaining > 0 {
        let maximum = if first {
            FIRST_PLAINTEXT_SEGMENT_SIZE + TAG_SIZE
        } else {
            PLAINTEXT_SEGMENT_SIZE + TAG_SIZE
        };
        let segment_len = remaining.min(maximum as u64) as usize;
        ensure!(
            segment_len >= TAG_SIZE,
            "truncated AES-GCM-HKDF segment in prefix"
        );
        let final_segment = remaining <= maximum as u64;
        let mut segment = Zeroizing::new(vec![0u8; segment_len]);
        input
            .read_exact(&mut segment)
            .await
            .with_context(|| format!("truncated AES-GCM-HKDF segment {index} in prefix"))?;
        let nonce = make_nonce(nonce_prefix, index, final_segment);
        let nonce = Nonce::from(nonce);
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(
                    &nonce,
                    Payload {
                        msg: &segment,
                        aad: &[],
                    },
                )
                .map_err(|_| anyhow!("authenticate AES-GCM-HKDF prefix segment {index}"))?,
        );
        drop(plaintext);
        processed += segment_len as u64;
        remaining -= segment_len as u64;
        if final_segment {
            if total_ciphertext_bytes <= prefix_limit + HEADER_SIZE as u64 {
                let mut trailing = [0u8; 1];
                ensure!(
                    input
                        .read(&mut trailing)
                        .await
                        .context("check AES-GCM-HKDF prefix boundary")?
                        == 0,
                    "AES-GCM-HKDF prefix contains trailing data"
                );
            }
            return Ok(());
        }
        first = false;
        index = index
            .checked_add(1)
            .context("AES-GCM-HKDF segment counter overflow")?;
    }

    ensure!(
        processed == prefix_limit,
        "encrypted object ended before two complete segments were available"
    );
    Ok(())
}

fn derive_key(master_key: &[u8; 32], salt: &[u8], aad: &[u8]) -> Result<Zeroizing<[u8; 16]>> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), master_key);
    let mut key = Zeroizing::new([0u8; 16]);
    hkdf.expand(aad, &mut *key)
        .map_err(|_| anyhow!("derive AES-GCM-HKDF key"))?;
    Ok(key)
}

fn make_nonce(prefix: &[u8], index: u32, is_last: bool) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..NONCE_PREFIX_SIZE].copy_from_slice(prefix);
    nonce[NONCE_PREFIX_SIZE..NONCE_PREFIX_SIZE + 4].copy_from_slice(&index.to_be_bytes());
    nonce[11] = u8::from(is_last);
    nonce
}

/// Resolve a selector to exactly one full fingerprint with an encryption-capable key.
pub async fn resolve_recipient(selector: &str) -> Result<String> {
    select_unique_fingerprint(&list_recipient_keys(selector).await?, true)
}

/// Resolve a selector to one primary fingerprint for decrypting historical backups.
///
/// Unlike [`resolve_recipient`], this lookup does not require the public key to be
/// currently encryption-capable. Expired, revoked, or encryption-ineligible keys may
/// still identify the matching historical secret key in the user's GPG keyring.
pub async fn resolve_decryption_recipient(selector: &str) -> Result<String> {
    select_unique_fingerprint(&list_recipient_keys(selector).await?, false)
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

fn parse_gpg_key_listing(text: &str) -> Vec<ListedRecipient> {
    let mut recipients = Vec::new();
    let mut seen = HashSet::new();
    let mut primary_fingerprint = None::<String>;
    let mut encryption_capable = false;
    let mut primary_is_current = false;
    let mut awaiting_primary_fingerprint = false;

    let finish_key = |fingerprint: &mut Option<String>,
                      capable: &mut bool,
                      recipients: &mut Vec<ListedRecipient>,
                      seen: &mut HashSet<String>| {
        if let Some(fingerprint) = fingerprint.take() {
            if seen.insert(fingerprint.clone()) {
                recipients.push(ListedRecipient {
                    fingerprint,
                    encryption_capable: *capable,
                });
            }
        }
    };

    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        match fields.first().copied() {
            Some("pub") => {
                finish_key(
                    &mut primary_fingerprint,
                    &mut encryption_capable,
                    &mut recipients,
                    &mut seen,
                );
                primary_fingerprint = None;
                primary_is_current = record_is_current(&fields);
                encryption_capable = primary_is_current
                    && fields
                        .get(11)
                        .is_some_and(|caps| caps.to_ascii_lowercase().contains('e'));
                awaiting_primary_fingerprint = true;
            }
            Some("sub") => {
                encryption_capable |= primary_is_current
                    && record_is_current(&fields)
                    && fields
                        .get(11)
                        .is_some_and(|caps| caps.to_ascii_lowercase().contains('e'));
                awaiting_primary_fingerprint = false;
            }
            Some("fpr") if awaiting_primary_fingerprint => {
                if let Some(fingerprint) = fields.get(9) {
                    primary_fingerprint = Some(fingerprint.to_ascii_uppercase());
                }
                awaiting_primary_fingerprint = false;
            }
            _ => {}
        }
    }
    finish_key(
        &mut primary_fingerprint,
        &mut encryption_capable,
        &mut recipients,
        &mut seen,
    );
    recipients
}

fn record_is_current(fields: &[&str]) -> bool {
    !fields.get(1).is_some_and(|validity| {
        ["e", "r", "d", "i"]
            .iter()
            .any(|invalid| validity.eq_ignore_ascii_case(invalid))
    })
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
    let fingerprint = recipient.fingerprint.clone();
    ensure!(
        (fingerprint.len() == 40 || fingerprint.len() == 64)
            && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "GPG returned an invalid primary fingerprint"
    );
    Ok(fingerprint)
}

/// Encrypt a 32-byte backup key to a public-key recipient using the user's GPG trust policy.
pub async fn encrypt_key(fingerprint: &str, key: &[u8; 32]) -> Result<Vec<u8>> {
    let fingerprint = validate_fingerprint(fingerprint)?;
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

/// Decrypt a GPG-protected backup key without placing plaintext on disk.
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

fn validate_fingerprint(fingerprint: &str) -> Result<String> {
    ensure!(
        (fingerprint.len() == 40 || fingerprint.len() == 64)
            && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "GPG recipient must be a full 40- or 64-character fingerprint"
    );
    Ok(fingerprint.to_ascii_uppercase())
}

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
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("spawn gpg")?;

    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped GPG stdout");
    let stderr = child.stderr.take().expect("piped GPG stderr");
    let stdout_task = collect_limited(stdout, stdout_limit);
    let stderr_task = collect_limited(stderr, MAX_GPG_STDERR_SIZE);
    let input_task = async move {
        match (stdin, input) {
            (Some(mut stdin), Some(bytes)) => stdin
                .write_all(bytes)
                .await
                .context("write GPG input")
                .map(|_| ()),
            (None, Some(_)) => bail!("GPG stdin pipe was not created"),
            (Some(mut stdin), None) => stdin.shutdown().await.context("close GPG stdin"),
            (None, None) => Ok(()),
        }
    };
    let (stdout_result, stderr_result, input_result, status_result) =
        tokio::join!(stdout_task, stderr_task, input_task, child.wait());
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

async fn collect_limited<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> io::Result<(Zeroizing<Vec<u8>>, bool)> {
    let mut collected = Zeroizing::new(Vec::with_capacity(limit.min(8192)));
    let mut overflow = false;
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let available = limit.saturating_sub(collected.len());
        let keep = available.min(count);
        collected.extend_from_slice(&buffer[..keep]);
        overflow |= keep != count;
        buffer[..count].zeroize();
    }
    Ok((collected, overflow))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        task::{Context as TaskContext, Poll},
    };
    #[derive(Default)]
    struct VecWriter(Vec<u8>);

    impl AsyncWrite for VecWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            input: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.0.extend_from_slice(input);
            Poll::Ready(Ok(input.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn matches_independent_tink_wire_reference() {
        let vector = hex::decode(include_str!("../tests/crypto_tink_vector.hex").trim()).unwrap();
        let plaintext = b"Tink-compatible reference payload";
        let aad = b"tink cross-language vector";
        let header: [u8; HEADER_SIZE] = vector[..HEADER_SIZE].try_into().unwrap();
        let mut input = &plaintext[..];
        let mut output = VecWriter::default();

        encrypt_with_header(
            &(std::array::from_fn(|index| index as u8)),
            aad,
            &mut input,
            &mut output,
            &header,
        )
        .await
        .unwrap();

        assert_eq!(output.0, vector);
    }

    #[test]
    fn historical_recipient_selection_ignores_expiry_and_encryption_capability() {
        let fixtures = [
            (
                include_str!("../tests/crypto_gpg_expired_key.colons"),
                "A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1A1",
            ),
            (
                include_str!("../tests/crypto_gpg_no_encrypt_key.colons"),
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
