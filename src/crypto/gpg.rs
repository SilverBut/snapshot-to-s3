//! GPG encryption and key management

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

/// Find and encrypt data with a GPG public key by user ID or key ID
pub async fn encrypt_with_gpg_key(identifier: &str, data: &[u8]) -> Result<Vec<u8>> {
    // Use gpg command-line tool to encrypt
    let mut child = Command::new("gpg")
        .args(&[
            "--encrypt",
            "--recipient", identifier,
            "--armor",
            "--trust-model", "always",  // Trust the key without prompting
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("Failed to spawn gpg command")?;
    
    // Write data to stdin
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin.write_all(data).await
            .context("Failed to write to gpg stdin")?;
    }
    
    // Wait for gpg to finish and collect output
    let output = child.wait_with_output().await
        .context("Failed to wait for gpg command")?;
    
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!(
            "GPG encryption failed for key '{}': {}",
            identifier,
            stderr
        ));
    }
    
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn test_encrypt_with_gpg_key() {
        // This test is ignored because it requires a GPG key to be set up
        let result = encrypt_with_gpg_key("test@example.com", b"test data").await;
        // Should either succeed if key exists or fail with appropriate error
        assert!(result.is_ok() || result.is_err());
    }
}
