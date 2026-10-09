//! Envelope encryption: a random data key per backup, wrapped by GPG.
//!
//! - [`stream`]: Tink `AES128_GCM_HKDF_1MB` streaming AEAD for objects.
//! - [`gpg`]: recipient resolution and data-key wrapping via `gpg`.

mod gpg;
mod stream;

use anyhow::{bail, ensure, Result};
use bytes::Bytes;
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub use gpg::{decrypt_key, encrypt_key, resolve_decryption_recipient, resolve_recipient};
pub use stream::{ciphertext_size, decrypt, encrypt, verify_prefix, VERIFY_PREFIX_BYTES};

/// Length of the `key.sha256sum` object: 64 lowercase hex digits and LF.
pub const CHECKSUM_SIZE: usize = 65;

/// Generates a random, zeroizing 256-bit data key.
pub fn generate_key() -> Zeroizing<[u8; 32]> {
    let mut key = Zeroizing::new([0u8; 32]);
    OsRng.fill_bytes(&mut *key);
    key
}

/// Returns the `key.sha256sum` content for `key`.
pub fn checksum(key: &[u8; 32]) -> Vec<u8> {
    format!("{}\n", hex::encode(Sha256::digest(key))).into_bytes()
}

/// Verifies the exact `key.sha256sum` content.
pub fn verify_checksum(key: &[u8; 32], supplied: &[u8]) -> Result<()> {
    ensure!(
        supplied == checksum(key),
        "key SHA-256 checksum does not match"
    );
    Ok(())
}

/// Encrypts a small in-memory object with empty associated data.
pub async fn encrypt_small(key: &[u8; 32], plaintext: &[u8]) -> Result<Bytes> {
    let mut output = Vec::with_capacity(ciphertext_size(plaintext.len() as u64) as usize);
    encrypt(key, &[], &mut &plaintext[..], &mut output).await?;
    Ok(Bytes::from(output))
}

/// Authenticates and decrypts a small object, rejecting plaintext over `limit`.
pub async fn decrypt_small(key: &[u8; 32], ciphertext: &[u8], limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    decrypt(key, &[], &mut &ciphertext[..], &mut output).await?;
    if output.len() > limit {
        bail!("decrypted object exceeds {limit}-byte limit");
    }
    Ok(output)
}
