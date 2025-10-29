//! AES-256-GCM encryption and decryption

use aes_gcm::{
    aead::{Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use rand::RngCore;
use std::io::Cursor;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Encrypt data with AES-256-GCM
pub async fn encrypt_stream<R>(
    key: &[u8; 32],
    reader: &mut R,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let cipher = Aes256Gcm::new_from_slice(key)
        .context("Failed to create cipher")?;
    
    // Read all data from stream
    let mut data = Vec::new();
    reader.read_to_end(&mut data).await.context("Failed to read stream")?;
    
    // Generate a random nonce
    let mut nonce_bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    
    // Encrypt the data
    let ciphertext = cipher
        .encrypt(nonce, data.as_ref())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;
    
    // Prepend nonce to ciphertext
    let mut result = nonce_bytes.to_vec();
    result.extend_from_slice(&ciphertext);
    
    Ok(result)
}

/// Decrypt data with AES-256-GCM
pub async fn decrypt_stream(
    key: &[u8; 32],
    encrypted_data: &[u8],
) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .context("Failed to create cipher")?;
    
    // Extract nonce from the beginning
    if encrypted_data.len() < 12 {
        return Err(anyhow::anyhow!("Data too short to contain nonce"));
    }
    
    let nonce = Nonce::from_slice(&encrypted_data[..12]);
    let ciphertext = &encrypted_data[12..];
    
    // Decrypt the data
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;
    
    Ok(plaintext)
}

/// Create an encrypting stream adapter
pub struct EncryptingStream<R> {
    reader: R,
    key: [u8; 32],
    buffer: Vec<u8>,
    position: usize,
}

impl<R: AsyncRead + Unpin> EncryptingStream<R> {
    pub fn new(reader: R, key: [u8; 32]) -> Self {
        Self {
            reader,
            key,
            buffer: Vec::new(),
            position: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for EncryptingStream<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        // If we have buffered encrypted data, return it
        if self.position < self.buffer.len() {
            let available = self.buffer.len() - self.position;
            let to_copy = std::cmp::min(available, buf.remaining());
            buf.put_slice(&self.buffer[self.position..self.position + to_copy]);
            self.position += to_copy;
            return std::task::Poll::Ready(Ok(()));
        }
        
        // Otherwise, we need to read more data and encrypt it
        // For simplicity, we'll read all remaining data and encrypt it
        // In a production implementation, this should be done in chunks
        let mut data = Vec::new();
        let mut inner_reader = std::pin::Pin::new(&mut self.reader);
        
        loop {
            let mut temp_buf = [0u8; 8192];
            let mut read_buf = tokio::io::ReadBuf::new(&mut temp_buf);
            
            match inner_reader.as_mut().poll_read(cx, &mut read_buf) {
                std::task::Poll::Ready(Ok(())) => {
                    let filled = read_buf.filled().len();
                    if filled == 0 {
                        break;
                    }
                    data.extend_from_slice(&temp_buf[..filled]);
                }
                std::task::Poll::Ready(Err(e)) => return std::task::Poll::Ready(Err(e)),
                std::task::Poll::Pending => return std::task::Poll::Pending,
            }
        }
        
        if data.is_empty() {
            return std::task::Poll::Ready(Ok(()));
        }
        
        // Encrypt the data
        let cipher = match Aes256Gcm::new_from_slice(&self.key) {
            Ok(c) => c,
            Err(e) => return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to create cipher: {}", e),
            ))),
        };
        
        let mut nonce_bytes = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        
        let ciphertext = match cipher.encrypt(nonce, data.as_ref()) {
            Ok(c) => c,
            Err(e) => return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Encryption failed: {}", e),
            ))),
        };
        
        // Prepend nonce to ciphertext
        self.buffer = nonce_bytes.to_vec();
        self.buffer.extend_from_slice(&ciphertext);
        self.position = 0;
        
        // Now return the encrypted data
        let available = self.buffer.len();
        let to_copy = std::cmp::min(available, buf.remaining());
        buf.put_slice(&self.buffer[..to_copy]);
        self.position += to_copy;
        
        std::task::Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_encrypt_decrypt() {
        let key = [0u8; 32];
        let data = b"Hello, World!";
        let mut reader = Cursor::new(data);
        
        let encrypted = encrypt_stream(&key, &mut reader).await.unwrap();
        assert!(encrypted.len() > data.len()); // Should be longer due to nonce and tag
        
        let decrypted = decrypt_stream(&key, &encrypted).await.unwrap();
        assert_eq!(decrypted, data);
    }
}
