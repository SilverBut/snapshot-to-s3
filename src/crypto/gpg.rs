//! GPG encryption and key management

use anyhow::{anyhow, Context, Result};
use pgp::{
    Deserializable, SignedPublicKey, crypto::sym::SymmetricKeyAlgorithm,
    types::{PublicKeyTrait, SecretKeyTrait},
};
use std::io::Cursor;

/// Find a public key by user ID or key ID
pub async fn find_public_key(identifier: &str) -> Result<SignedPublicKey> {
    // For now, this is a placeholder. In a real implementation, this would:
    // 1. Search in the user's GPG keyring
    // 2. Parse GPG keys from standard locations
    // 3. Support both key IDs and user IDs
    
    Err(anyhow!("GPG key lookup not yet implemented. Please provide the key file directly."))
}

/// Load a public key from a file
pub async fn load_public_key_from_file(path: &str) -> Result<SignedPublicKey> {
    let key_data = tokio::fs::read(path).await
        .context("Failed to read public key file")?;
    
    let (key, _) = SignedPublicKey::from_armor_single(Cursor::new(&key_data))
        .context("Failed to parse armored public key")?;
    
    Ok(key)
}

/// Encrypt data with a GPG public key
pub async fn encrypt_with_public_key(
    public_key: &SignedPublicKey,
    data: &[u8],
) -> Result<Vec<u8>> {
    use pgp::composed::{Deserializable, Message};
    use pgp::crypto::hash::HashAlgorithm;
    use pgp::types::CompressionAlgorithm;
    
    // Convert data to string (for literal message)
    let data_str = String::from_utf8_lossy(data);
    
    // Create a message from the data
    let message = Message::new_literal("data", &data_str);
    
    // Encrypt the message
    let mut rng = rand::thread_rng();
    let encrypted = message
        .encrypt_to_keys_seipdv1(
            &mut rng,
            SymmetricKeyAlgorithm::AES256,
            &[public_key],
        )
        .context("Failed to encrypt with public key")?;
    
    // Serialize to bytes
    let mut encrypted_bytes = Vec::new();
    encrypted.to_armored_writer(&mut encrypted_bytes, Default::default())
        .context("Failed to armor encrypted message")?;
    
    Ok(encrypted_bytes)
}

/// Decrypt data with a GPG private key
pub async fn decrypt_with_private_key(
    encrypted_data: &[u8],
    private_key_path: &str,
    passphrase: Option<&str>,
) -> Result<Vec<u8>> {
    use pgp::composed::{Deserializable, Message, SignedSecretKey};
    
    // Load the private key
    let key_data = tokio::fs::read(private_key_path).await
        .context("Failed to read private key file")?;
    
    let (secret_key, _) = SignedSecretKey::from_armor_single(Cursor::new(&key_data))
        .context("Failed to parse armored private key")?;
    
    // Parse the encrypted message
    let (message, _) = Message::from_armor_single(Cursor::new(encrypted_data))
        .context("Failed to parse encrypted message")?;
    
    // Decrypt the message
    let (decrypted, _) = message
        .decrypt(|| passphrase.unwrap_or("").to_string(), &[&secret_key])
        .context("Failed to decrypt message")?;
    
    // Extract the literal data
    match decrypted {
        pgp::composed::Message::Literal(literal) => {
            Ok(literal.data().to_vec())
        }
        _ => Err(anyhow!("Decrypted message is not a literal message")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require actual GPG keys to work
    // They are here as examples of how to use the API
    
    #[tokio::test]
    #[ignore]
    async fn test_encrypt_decrypt_roundtrip() {
        // This test is ignored because it requires real GPG keys
        // To run it, provide actual key files and remove the #[ignore] attribute
        let public_key_path = "test_public.asc";
        let private_key_path = "test_private.asc";
        let data = b"Test data for encryption";
        
        let public_key = load_public_key_from_file(public_key_path).await.unwrap();
        let encrypted = encrypt_with_public_key(&public_key, data).await.unwrap();
        let decrypted = decrypt_with_private_key(&encrypted, private_key_path, None).await.unwrap();
        
        assert_eq!(decrypted, data);
    }
}
