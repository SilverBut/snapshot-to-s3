//! Upload pipeline: `zfs send` → encryption → rate limit → multipart parts.
//!
//! Encryption and rate limiting run as tasks connected by bounded duplex
//! pipes, so memory use is bounded by the pipe sizes and one part buffer.

use super::Job;
use crate::crypto;
use crate::model::Reader;
use crate::store::{upload_parts, UploadedParts};
use crate::zfs::SendStream;
use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use zeroize::Zeroizing;

const ENCRYPTED_PIPE_BYTES: usize = 2 * 1024 * 1024;
const RATE_PIPE_BYTES: usize = 64 * 1024;

impl Job<'_> {
    /// Uploads the encrypted send stream as parts of `upload_id`. The
    /// estimate only sizes parts.
    pub(super) async fn upload_stream(
        &self,
        upload_id: &str,
        send: SendStream,
        key: &Zeroizing<[u8; 32]>,
        aad: [u8; 32],
        ciphertext_estimate: u64,
    ) -> Result<UploadedParts> {
        let SendStream {
            reader: mut plaintext,
            completion: send_completion,
            cancel: send_cancel,
        } = send;
        let (encrypted_reader, mut encrypted_writer) = tokio::io::duplex(ENCRYPTED_PIPE_BYTES);
        let key = key.clone();
        let encryption = tokio::spawn(async move {
            crypto::encrypt(&key, &aad, &mut plaintext, &mut encrypted_writer).await?;
            encrypted_writer.shutdown().await?;
            anyhow::Ok(())
        });
        let (limited_reader, mut limited_writer) = tokio::io::duplex(RATE_PIPE_BYTES);
        let rate_limit = self.options.rate_limit;
        let limiter = tokio::spawn(async move {
            let mut input = encrypted_reader;
            crate::rate::copy_limited(&mut input, &mut limited_writer, rate_limit).await?;
            limited_writer.shutdown().await?;
            anyhow::Ok(())
        });

        let mut ciphertext: Reader = Box::new(limited_reader);
        let parts = upload_parts(
            self.store,
            &self.stream_key,
            upload_id,
            &mut ciphertext,
            ciphertext_estimate,
            &self.options.limits,
            &self.options.cancel,
        )
        .await;
        if parts.is_err() {
            send_cancel.cancel();
            encryption.abort();
            limiter.abort();
        }
        drop(ciphertext);
        let encryption = encryption.await;
        let limiter = limiter.await;
        let send = send_completion.await;
        let parts = match parts {
            Ok(parts) => parts,
            Err(error) => {
                eprintln!(
                    "producer shutdown after upload failure: encryption={encryption:?}, \
                     limiter={limiter:?}, send={send:?}"
                );
                return Err(error);
            }
        };
        encryption.context("encryption task failed")??;
        limiter.context("rate limiter task failed")??;
        send.context("send monitor failed")??;
        Ok(parts)
    }
}
