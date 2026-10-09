//! Tink-compatible `AES128_GCM_HKDF_1MB` streaming AEAD.
//!
//! Wire format: a 24-byte header (`0x18`, 16-byte HKDF salt, 7-byte nonce
//! prefix) followed by 1 MiB ciphertext segments, each with a 16-byte tag.
//! The first segment is shortened by the header. Segment nonces are
//! `prefix || index (u32 BE) || last-segment flag`, so truncation and
//! reordering fail authentication. The segment key is
//! `HKDF-SHA256(salt, data key, info = AAD)`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{anyhow, bail, ensure, Context, Result};
use hkdf::Hkdf;
use rand::{rngs::OsRng, RngCore};
use sha2::Sha256;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::{Zeroize, Zeroizing};

pub(crate) const HEADER_SIZE: usize = 24;
const SALT_SIZE: usize = 16;
const NONCE_PREFIX_SIZE: usize = 7;
const SALT_OFFSET: usize = 1;
const NONCE_PREFIX_OFFSET: usize = SALT_OFFSET + SALT_SIZE;
const TAG_SIZE: usize = 16;
const CIPHERTEXT_SEGMENT_SIZE: usize = 1_048_576;
const FIRST_PLAINTEXT_SEGMENT_SIZE: usize = CIPHERTEXT_SEGMENT_SIZE - HEADER_SIZE - TAG_SIZE;
const PLAINTEXT_SEGMENT_SIZE: usize = CIPHERTEXT_SEGMENT_SIZE - TAG_SIZE;
const READ_CHUNK_SIZE: usize = 8192;

/// Ciphertext prefix that [`verify_prefix`] authenticates: two segments.
pub const VERIFY_PREFIX_BYTES: u64 = 2 * CIPHERTEXT_SEGMENT_SIZE as u64;

/// Exact ciphertext length for `plaintext_len` bytes (saturating).
pub fn ciphertext_size(plaintext_len: u64) -> u64 {
    let first = FIRST_PLAINTEXT_SEGMENT_SIZE as u64;
    let segments = 1 + plaintext_len
        .saturating_sub(first)
        .div_ceil(PLAINTEXT_SEGMENT_SIZE as u64);
    (HEADER_SIZE as u64)
        .saturating_add(plaintext_len)
        .saturating_add(segments.saturating_mul(TAG_SIZE as u64))
}

/// Encrypts an arbitrarily long stream with a random header.
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
    let cipher = SegmentCipher::new(key, header, aad)?;
    output
        .write_all(header)
        .await
        .context("write encryption header")?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(PLAINTEXT_SEGMENT_SIZE));
    let mut carry = None;
    let mut index = 0u32;
    loop {
        let last = read_segment(input, plaintext_capacity(index), &mut carry, &mut plaintext)
            .await
            .context("read plaintext stream")?;
        if !last && index == u32::MAX {
            bail!("AES-GCM-HKDF stream exceeds the segment counter limit");
        }
        let ciphertext = cipher
            .seal(index, last, &plaintext)
            .map_err(|_| anyhow!("encrypt AES-GCM-HKDF segment"))?;
        output
            .write_all(&ciphertext)
            .await
            .context("write encrypted segment")?;
        if last {
            break;
        }
        index += 1;
    }
    output.flush().await.context("flush encrypted stream")
}

/// Authenticates and decrypts every segment, including the final segment.
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
    let cipher = SegmentCipher::new(key, &header, aad)?;
    let mut segment = Vec::with_capacity(CIPHERTEXT_SEGMENT_SIZE);
    let mut carry = None;
    let mut index = 0u32;
    loop {
        let capacity = plaintext_capacity(index) + TAG_SIZE;
        let last = read_segment(input, capacity, &mut carry, &mut segment)
            .await
            .context("read encrypted segment")?;
        if segment.is_empty() {
            bail!("encrypted stream has no authenticated final segment");
        }
        ensure!(
            segment.len() >= TAG_SIZE,
            "truncated AES-GCM-HKDF segment tag"
        );
        let plaintext = cipher
            .open(index, last, &segment)
            .map_err(|_| anyhow!("authenticate AES-GCM-HKDF segment {index}"))?;
        output
            .write_all(&plaintext)
            .await
            .context("write authenticated plaintext segment")?;
        if last {
            break;
        }
        index = index
            .checked_add(1)
            .context("AES-GCM-HKDF segment counter overflow")?;
    }
    output.flush().await.context("flush decrypted stream")
}

/// Authenticates the complete initial segments for restore preflight.
///
/// `input` must expose the first `min(total_ciphertext_bytes,
/// VERIFY_PREFIX_BYTES)` bytes of the object, such as an HTTP range
/// response. The known object size determines whether the last inspected
/// segment is final; no EOF is required for a non-final prefix.
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
    let cipher = SegmentCipher::new(key, &header, aad)?;
    let prefix_limit = VERIFY_PREFIX_BYTES - HEADER_SIZE as u64;
    let mut remaining = total_ciphertext_bytes - HEADER_SIZE as u64;
    let mut processed = 0u64;
    let mut index = 0u32;
    while processed < prefix_limit && remaining > 0 {
        let capacity = (plaintext_capacity(index) + TAG_SIZE) as u64;
        let segment_len = remaining.min(capacity) as usize;
        ensure!(
            segment_len >= TAG_SIZE,
            "truncated AES-GCM-HKDF segment in prefix"
        );
        let last = remaining <= capacity;
        let mut segment = Zeroizing::new(vec![0u8; segment_len]);
        input
            .read_exact(&mut segment)
            .await
            .with_context(|| format!("truncated AES-GCM-HKDF segment {index} in prefix"))?;
        cipher
            .open(index, last, &segment)
            .map_err(|_| anyhow!("authenticate AES-GCM-HKDF prefix segment {index}"))?;
        processed += segment_len as u64;
        remaining -= segment_len as u64;
        if last {
            if total_ciphertext_bytes <= VERIFY_PREFIX_BYTES {
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
        index += 1;
    }
    ensure!(
        processed == prefix_limit,
        "encrypted object ended before two complete segments were available"
    );
    Ok(())
}

/// Plaintext capacity of segment `index` when it is not the last.
fn plaintext_capacity(index: u32) -> usize {
    if index == 0 {
        FIRST_PLAINTEXT_SEGMENT_SIZE
    } else {
        PLAINTEXT_SEGMENT_SIZE
    }
}

/// Reads up to `capacity` bytes into `segment`, then peeks one byte to
/// learn whether this segment is the last one. A peeked byte is kept in
/// `carry` for the next segment.
async fn read_segment<R>(
    input: &mut R,
    capacity: usize,
    carry: &mut Option<u8>,
    segment: &mut Vec<u8>,
) -> std::io::Result<bool>
where
    R: AsyncRead + Unpin,
{
    segment.clear();
    segment.extend(carry.take());
    let mut chunk = Zeroizing::new([0u8; READ_CHUNK_SIZE]);
    while segment.len() < capacity {
        let limit = (capacity - segment.len()).min(READ_CHUNK_SIZE);
        let count = input.read(&mut chunk[..limit]).await?;
        if count == 0 {
            return Ok(true);
        }
        segment.extend_from_slice(&chunk[..count]);
    }
    let mut lookahead = [0u8; 1];
    if input.read(&mut lookahead).await? == 0 {
        return Ok(true);
    }
    *carry = Some(lookahead[0]);
    lookahead.zeroize();
    Ok(false)
}

/// Per-stream AES-128-GCM key and nonce prefix.
struct SegmentCipher {
    cipher: Aes128Gcm,
    nonce_prefix: [u8; NONCE_PREFIX_SIZE],
}

impl SegmentCipher {
    fn new(key: &[u8; 32], header: &[u8; HEADER_SIZE], aad: &[u8]) -> Result<Self> {
        ensure!(
            header[0] as usize == HEADER_SIZE,
            "invalid Tink streaming header length"
        );
        let salt = &header[SALT_OFFSET..NONCE_PREFIX_OFFSET];
        let mut segment_key = Zeroizing::new([0u8; 16]);
        Hkdf::<Sha256>::new(Some(salt), key)
            .expand(aad, &mut *segment_key)
            .map_err(|_| anyhow!("derive AES-GCM-HKDF key"))?;
        Ok(Self {
            cipher: Aes128Gcm::new_from_slice(&segment_key[..]).expect("fixed AES-128 key size"),
            nonce_prefix: header[NONCE_PREFIX_OFFSET..]
                .try_into()
                .expect("fixed nonce prefix size"),
        })
    }

    fn nonce(&self, index: u32, last: bool) -> Nonce<aes_gcm::aead::consts::U12> {
        let mut nonce = [0u8; 12];
        nonce[..NONCE_PREFIX_SIZE].copy_from_slice(&self.nonce_prefix);
        nonce[NONCE_PREFIX_SIZE..NONCE_PREFIX_SIZE + 4].copy_from_slice(&index.to_be_bytes());
        nonce[11] = u8::from(last);
        Nonce::from(nonce)
    }

    fn seal(&self, index: u32, last: bool, plaintext: &[u8]) -> aes_gcm::aead::Result<Vec<u8>> {
        self.cipher.encrypt(&self.nonce(index, last), plaintext)
    }

    fn open(
        &self,
        index: u32,
        last: bool,
        ciphertext: &[u8],
    ) -> aes_gcm::aead::Result<Zeroizing<Vec<u8>>> {
        self.cipher
            .decrypt(&self.nonce(index, last), ciphertext)
            .map(Zeroizing::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matches_independent_tink_wire_reference() {
        let vector =
            hex::decode(include_str!("../../tests/fixtures/tink_vector.hex").trim()).unwrap();
        let plaintext = b"Tink-compatible reference payload";
        let aad = b"tink cross-language vector";
        let header: [u8; HEADER_SIZE] = vector[..HEADER_SIZE].try_into().unwrap();
        let mut output = Vec::new();
        encrypt_with_header(
            &(std::array::from_fn(|index| index as u8)),
            aad,
            &mut &plaintext[..],
            &mut output,
            &header,
        )
        .await
        .unwrap();
        assert_eq!(output, vector);
    }
}
