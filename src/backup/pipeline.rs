//! Upload pipeline: `zfs send` → encryption → rate limit → multipart parts.
//!
//! Encryption and rate limiting run as tasks connected by bounded duplex
//! pipes, so memory use is bounded by the pipe sizes and one part buffer,
//! whatever the stream size. A stream larger than one object continues in
//! completed continuation objects; `stream.encrypted` stays open and is
//! completed last by the caller.

use super::Job;
use crate::crypto;
use crate::model::object;
use crate::store::{upload_object, upload_parts, UploadedParts};
use crate::zfs::SendStream;
use anyhow::{ensure, Context, Result};
use tokio::io::{AsyncBufRead, AsyncWriteExt, BufReader};
use zeroize::Zeroizing;

const ENCRYPTED_PIPE_BYTES: usize = 2 * 1024 * 1024;
const RATE_PIPE_BYTES: usize = 64 * 1024;

/// The uploaded ciphertext of one backup.
pub(super) struct UploadedStream {
    /// Parts of the still-open `stream.encrypted`.
    pub(super) head: UploadedParts,
    /// Objects, including `stream.encrypted`.
    pub(super) objects: u32,
    /// Ciphertext bytes over all objects.
    pub(super) bytes: u64,
    pub(super) peak_buffer_bytes: usize,
}

impl Job<'_> {
    /// Uploads the encrypted send stream, starting with the parts of
    /// `upload_id`. The estimate only sizes parts.
    pub(super) async fn upload_stream(
        &self,
        upload_id: &str,
        send: SendStream,
        key: &Zeroizing<[u8; 32]>,
        aad: [u8; 32],
        ciphertext_estimate: u64,
    ) -> Result<UploadedStream> {
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

        let mut ciphertext = BufReader::with_capacity(RATE_PIPE_BYTES, limited_reader);
        let uploaded = self
            .upload_objects(upload_id, &mut ciphertext, ciphertext_estimate)
            .await;
        if uploaded.is_err() {
            send_cancel.cancel();
            encryption.abort();
            limiter.abort();
        }
        drop(ciphertext);
        let encryption = encryption.await;
        let limiter = limiter.await;
        let send = send_completion.await;
        let uploaded = match uploaded {
            Ok(uploaded) => uploaded,
            Err(error) => {
                tracing::warn!(
                    "producer shutdown after upload failure: encryption={encryption:?}, \
                     limiter={limiter:?}, send={send:?}"
                );
                return Err(error);
            }
        };
        encryption.context("encryption task failed")??;
        limiter.context("rate limiter task failed")??;
        send.context("send monitor failed")??;
        Ok(uploaded)
    }

    /// Fills `stream.encrypted`, then continuation objects until the
    /// ciphertext ends.
    async fn upload_objects<R: AsyncBufRead + Unpin>(
        &self,
        upload_id: &str,
        ciphertext: &mut R,
        estimate: u64,
    ) -> Result<UploadedStream> {
        let options = self.options;
        let head = upload_parts(
            self.store,
            &self.stream_key,
            upload_id,
            ciphertext,
            estimate,
            &options.limits,
            &options.cancel,
        )
        .await?;
        let mut stream = UploadedStream {
            objects: 1,
            bytes: head.bytes,
            peak_buffer_bytes: head.peak_buffer_bytes,
            head,
        };
        let mut ended = stream.head.ended;
        while !ended {
            ensure!(
                stream.objects <= object::MAX_CONTINUATIONS,
                "stream exceeds {} continuation objects",
                object::MAX_CONTINUATIONS
            );
            let key = object::continuation(&self.stream_key, stream.objects);
            // Past the estimate, assume the rest is large: use the largest parts.
            let remaining = Some(estimate.saturating_sub(stream.bytes))
                .filter(|remaining| *remaining > 0)
                .unwrap_or(u64::MAX);
            let uploaded = upload_object(
                self.store,
                &key,
                ciphertext,
                remaining,
                &options.limits,
                &options.cancel,
            )
            .await
            .with_context(|| format!("upload continuation object {key}"))?;
            stream.objects += 1;
            stream.bytes = stream
                .bytes
                .checked_add(uploaded.bytes)
                .context("ciphertext length overflow")?;
            stream.peak_buffer_bytes = stream.peak_buffer_bytes.max(uploaded.peak_buffer_bytes);
            ended = uploaded.ended;
        }
        Ok(stream)
    }
}
