//! Cryptography module
//!
//! This module provides encryption and decryption functionality.

pub mod aes;
pub mod gpg;

use anyhow::Result;
use rand::Rng;

/// Generate a random 256-bit key
pub fn generate_key() -> [u8; 32] {
    let mut rng = rand::thread_rng();
    let mut key = [0u8; 32];
    rng.fill(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_key() {
        let key1 = generate_key();
        let key2 = generate_key();
        assert_eq!(key1.len(), 32);
        assert_eq!(key2.len(), 32);
        // Keys should be different
        assert_ne!(key1, key2);
    }
}
