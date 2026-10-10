//! Where a multipart part is held before upload: in memory, or in one
//! exclusively created temporary file that only ever holds ciphertext.

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

/// Bytes copied per read when spooling a part to its file.
const FILE_CHUNK_BYTES: usize = 1024 * 1024;

/// Where parts are held. A part never exceeds `--max-part-size` either way.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum PartStorage {
    #[default]
    Memory,
    /// A path that must not exist; created with mode 0600, reused for every
    /// part and deleted afterwards.
    TempFile(PathBuf),
}

/// One part's bytes, ready to upload any number of times.
#[derive(Clone, Debug)]
pub enum PartBody {
    Memory(Bytes),
    File(FilePart),
}

/// A part spooled to the temporary file.
#[derive(Clone, Debug)]
pub struct FilePart {
    file: Arc<std::fs::File>,
    len: u64,
    sha256: [u8; 32],
}

impl FilePart {
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// SHA-256 of the part, computed while it was written.
    pub fn sha256(&self) -> [u8; 32] {
        self.sha256
    }

    /// Reads the part from its start; each call starts over.
    pub async fn reader(&self) -> Result<impl AsyncRead + Send + Sync + Unpin + 'static> {
        let mut file = tokio::fs::File::from_std(
            self.file
                .try_clone()
                .context("reopen part temporary file")?,
        );
        file.seek(SeekFrom::Start(0))
            .await
            .context("seek part temporary file")?;
        Ok(file.take(self.len))
    }
}

impl PartBody {
    pub fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::File(part) => part.len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The whole part in memory.
    pub async fn into_bytes(self) -> Result<Bytes> {
        match self {
            Self::Memory(bytes) => Ok(bytes),
            Self::File(part) => {
                let mut bytes = Vec::with_capacity(usize::try_from(part.len)?);
                part.reader().await?.read_to_end(&mut bytes).await?;
                if bytes.len() as u64 != part.len {
                    bail!("part temporary file is shorter than the spooled part");
                }
                Ok(Bytes::from(bytes))
            }
        }
    }
}

/// Holds the part being filled; deletes its temporary file when dropped.
pub(super) enum Spool {
    Memory,
    File {
        path: PathBuf,
        file: Arc<std::fs::File>,
    },
}

impl Spool {
    pub(super) fn open(storage: &PartStorage) -> Result<Self> {
        let path = match storage {
            PartStorage::Memory => return Ok(Self::Memory),
            PartStorage::TempFile(path) => path,
        };
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path).with_context(|| {
            format!(
                "create part temporary file {} (the path must not exist)",
                path.display()
            )
        })?;
        tracing::debug!("spooling parts to {}", path.display());
        Ok(Self::File {
            path: path.clone(),
            file: Arc::new(file),
        })
    }

    /// Reads up to `size` bytes; a shorter part means the stream ended.
    pub(super) async fn fill<R: AsyncBufRead + Unpin + ?Sized>(
        &mut self,
        reader: &mut R,
        size: usize,
        cancel: &CancellationToken,
    ) -> Result<PartBody> {
        match self {
            Self::Memory => {
                let mut buffer = vec![0u8; size];
                let filled = read_into(reader, &mut buffer, cancel).await?;
                buffer.truncate(filled);
                Ok(PartBody::Memory(Bytes::from(buffer)))
            }
            Self::File { file, .. } => {
                file.set_len(0).context("truncate part temporary file")?;
                let mut out = tokio::fs::File::from_std(
                    file.try_clone().context("reopen part temporary file")?,
                );
                out.seek(SeekFrom::Start(0)).await?;
                let mut hasher = Sha256::new();
                let mut chunk = vec![0u8; size.clamp(1, FILE_CHUNK_BYTES)];
                let mut filled = 0usize;
                while filled < size {
                    let want = (size - filled).min(chunk.len());
                    let n = read_into(reader, &mut chunk[..want], cancel).await?;
                    if n == 0 {
                        break;
                    }
                    hasher.update(&chunk[..n]);
                    out.write_all(&chunk[..n])
                        .await
                        .context("write part temporary file")?;
                    filled += n;
                    if n < want {
                        break;
                    }
                }
                out.flush().await.context("flush part temporary file")?;
                Ok(PartBody::File(FilePart {
                    file: file.clone(),
                    len: filled as u64,
                    sha256: hasher.finalize().into(),
                }))
            }
        }
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if let Self::File { path, .. } = self {
            if let Err(error) = std::fs::remove_file(&*path) {
                tracing::warn!(
                    "cannot delete part temporary file {}: {error}",
                    path.display()
                );
            }
        }
    }
}

/// Fills `buffer` until it is full or the reader ends; returns the length.
async fn read_into<R: AsyncBufRead + Unpin + ?Sized>(
    reader: &mut R,
    buffer: &mut [u8],
    cancel: &CancellationToken,
) -> Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let n = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("backup cancelled"),
            result = reader.read(&mut buffer[filled..]) => {
                result.context("read encrypted stream")?
            }
        };
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_spool_is_exclusive_private_reusable_and_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part");
        let storage = PartStorage::TempFile(path.clone());
        let cancel = CancellationToken::new();
        {
            let mut spool = Spool::open(&storage).unwrap();
            assert!(Spool::open(&storage).is_err(), "path must be exclusive");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&path).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600);
            }
            let input: Vec<u8> = (0..=255).cycle().take(3 * FILE_CHUNK_BYTES).collect();
            let mut reader = &input[..];
            let first = spool
                .fill(&mut reader, 2 * FILE_CHUNK_BYTES + 7, &cancel)
                .await
                .unwrap();
            assert_eq!(first.len(), 2 * FILE_CHUNK_BYTES as u64 + 7);
            let PartBody::File(part) = &first else {
                panic!("file storage must spool to the file")
            };
            assert_eq!(
                part.sha256(),
                <[u8; 32]>::from(Sha256::digest(&input[..2 * FILE_CHUNK_BYTES + 7]))
            );
            // Rereading starts over each time, as retries need.
            assert_eq!(
                first.clone().into_bytes().await.unwrap(),
                &input[..first.len() as usize]
            );
            assert_eq!(
                first.into_bytes().await.unwrap().len(),
                2 * FILE_CHUNK_BYTES + 7
            );
            // The next part replaces the file contents.
            let second = spool
                .fill(&mut reader, 2 * FILE_CHUNK_BYTES, &cancel)
                .await
                .unwrap();
            assert_eq!(
                second.into_bytes().await.unwrap(),
                &input[2 * FILE_CHUNK_BYTES + 7..]
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                (FILE_CHUNK_BYTES - 7) as u64
            );
        }
        assert!(!path.exists(), "dropping the spool deletes the file");
    }

    #[tokio::test]
    async fn file_spool_is_deleted_after_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part");
        let cancel = CancellationToken::new();
        cancel.cancel();
        {
            let mut spool = Spool::open(&PartStorage::TempFile(path.clone())).unwrap();
            assert!(spool.fill(&mut &b"data"[..], 4, &cancel).await.is_err());
        }
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn existing_path_is_refused_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("taken");
        std::fs::write(&path, b"user data").unwrap();
        assert!(Spool::open(&PartStorage::TempFile(path.clone())).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"user data");
    }
}
