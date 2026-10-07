use snapshot_to_s3::crypto;

use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const FIRST_SEGMENT_PLAINTEXT: usize = 1_048_576 - 24 - 16;
const SEGMENT_PLAINTEXT: usize = 1_048_576 - 16;

struct ChunkReader {
    bytes: Vec<u8>,
    offset: usize,
    chunk_size: usize,
    pending: bool,
}

impl ChunkReader {
    fn new(bytes: Vec<u8>, chunk_size: usize) -> Self {
        Self {
            bytes,
            offset: 0,
            chunk_size,
            pending: true,
        }
    }
}

impl AsyncRead for ChunkReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.pending {
            self.pending = false;
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        self.pending = true;
        let count = self
            .chunk_size
            .min(output.remaining())
            .min(self.bytes.len().saturating_sub(self.offset));
        if count != 0 {
            let start = self.offset;
            output.put_slice(&self.bytes[start..start + count]);
            self.offset += count;
        }
        Poll::Ready(Ok(()))
    }
}

#[derive(Default)]
struct VecWriter(Vec<u8>);

impl AsyncWrite for VecWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.0.extend_from_slice(input);
        Poll::Ready(Ok(input.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn key() -> [u8; 32] {
    std::array::from_fn(|index| index as u8)
}

async fn encrypt_bytes(plaintext: &[u8]) -> Vec<u8> {
    let mut reader = ChunkReader::new(plaintext.to_vec(), 7919);
    let mut writer = VecWriter::default();
    crypto::encrypt(&key(), b"metadata-hash", &mut reader, &mut writer)
        .await
        .unwrap();
    writer.0
}

async fn decrypt_bytes(ciphertext: &[u8], aad: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut reader = ChunkReader::new(ciphertext.to_vec(), 3701);
    let mut writer = VecWriter::default();
    crypto::decrypt(&key(), aad, &mut reader, &mut writer).await?;
    Ok(writer.0)
}

#[tokio::test]
async fn round_trips_empty_short_and_segment_boundaries() {
    let sizes = [
        0,
        1,
        FIRST_SEGMENT_PLAINTEXT - 1,
        FIRST_SEGMENT_PLAINTEXT,
        FIRST_SEGMENT_PLAINTEXT + 1,
        FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT - 1,
        FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT,
        FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT + 1,
    ];
    for size in sizes {
        let plaintext = vec![0x5a; size];
        let ciphertext = encrypt_bytes(&plaintext).await;
        assert_eq!(
            decrypt_bytes(&ciphertext, b"metadata-hash").await.unwrap(),
            plaintext,
            "plaintext length {size}"
        );
    }
}

#[tokio::test]
async fn interoperates_with_independent_tink_framing_vector() {
    let vector = hex::decode(include_str!("crypto_tink_vector.hex").trim()).unwrap();
    let plaintext = b"Tink-compatible reference payload";
    let aad = b"tink cross-language vector";
    assert_eq!(
        &vector[..24],
        &[0x18, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22]
    );
    assert_eq!(decrypt_bytes(&vector, aad).await.unwrap(), plaintext);
}

#[tokio::test]
async fn decrypts_ciphertext_generated_by_official_tink_python_runtime() {
    let vector = hex::decode(include_str!("crypto_tink_runtime_vector.hex").trim()).unwrap();
    assert_eq!(
        decrypt_bytes(&vector, b"tink cross-language vector")
            .await
            .unwrap(),
        b"Tink-compatible reference payload"
    );
}

#[tokio::test]
async fn authenticates_aad_segments_and_final_segment() {
    let plaintext = vec![0x31; FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT + 5];
    let ciphertext = encrypt_bytes(&plaintext).await;

    assert!(decrypt_bytes(&ciphertext, b"wrong aad").await.is_err());

    let mut damaged = ciphertext.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 1;
    assert!(decrypt_bytes(&damaged, b"metadata-hash").await.is_err());

    assert!(
        decrypt_bytes(&ciphertext[..ciphertext.len() - 1], b"metadata-hash")
            .await
            .is_err()
    );

    let mut trailing = ciphertext;
    trailing.push(0);
    assert!(decrypt_bytes(&trailing, b"metadata-hash").await.is_err());
}

#[tokio::test]
async fn verifies_prefix_with_known_object_length() {
    let small = encrypt_bytes(b"a short complete stream").await;
    let mut small_prefix = ChunkReader::new(small.clone(), 23);
    crypto::verify_prefix(
        &key(),
        b"metadata-hash",
        &mut small_prefix,
        small.len() as u64,
    )
    .await
    .unwrap();

    let plaintext = vec![0xa3; FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT * 3];
    let ciphertext = encrypt_bytes(&plaintext).await;
    let prefix_len = (2 * 1_048_576).min(ciphertext.len());
    let mut range = ChunkReader::new(ciphertext[..prefix_len].to_vec(), 4093);
    crypto::verify_prefix(
        &key(),
        b"metadata-hash",
        &mut range,
        ciphertext.len() as u64,
    )
    .await
    .unwrap();

    let complete_two_segments =
        encrypt_bytes(&vec![0x44; FIRST_SEGMENT_PLAINTEXT + SEGMENT_PLAINTEXT]).await;
    assert_eq!(complete_two_segments.len(), 2 * 1_048_576);
    let mut range = ChunkReader::new(complete_two_segments[..2 * 1_048_576].to_vec(), 8191);
    crypto::verify_prefix(
        &key(),
        b"metadata-hash",
        &mut range,
        complete_two_segments.len() as u64,
    )
    .await
    .unwrap();

    let mut short_range =
        ChunkReader::new(complete_two_segments[..2 * 1_048_576 - 1].to_vec(), 8191);
    assert!(crypto::verify_prefix(
        &key(),
        b"metadata-hash",
        &mut short_range,
        complete_two_segments.len() as u64,
    )
    .await
    .is_err());

    let mut corrupted_prefix = ciphertext[..prefix_len].to_vec();
    corrupted_prefix[1_048_575] ^= 1;
    let mut range = ChunkReader::new(corrupted_prefix, 2047);
    assert!(crypto::verify_prefix(
        &key(),
        b"metadata-hash",
        &mut range,
        ciphertext.len() as u64,
    )
    .await
    .is_err());
}

#[tokio::test]
async fn gpg_apis_reject_invalid_inputs_without_invoking_gpg() {
    assert!(crypto::resolve_recipient("-output").await.is_err());
    assert!(crypto::resolve_decryption_recipient("-output")
        .await
        .is_err());
    assert!(crypto::encrypt_key("not-a-full-fingerprint", &key())
        .await
        .is_err());
    assert!(crypto::decrypt_key(&[]).await.is_err());
}

#[test]
fn key_checksum_is_exact_and_lowercase() {
    let key = key();
    let checksum = crypto::checksum(&key);
    assert_eq!(checksum.len(), 65);
    assert_eq!(checksum[64], b'\n');
    assert!(checksum[..64]
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)));
    crypto::verify_checksum(&key, &checksum).unwrap();

    let mut uppercase = checksum.clone();
    let letter = uppercase
        .iter()
        .position(|byte| (b'a'..=b'f').contains(byte))
        .unwrap();
    uppercase[letter] = uppercase[letter].to_ascii_uppercase();
    assert!(crypto::verify_checksum(&key, &uppercase).is_err());
    assert!(crypto::verify_checksum(&key, &checksum[..64]).is_err());
    assert!(crypto::verify_checksum(&key, b"").is_err());
}

#[test]
fn generated_key_has_expected_size() {
    let key = crypto::generate_key();
    assert_eq!(key.len(), 32);
}
