//! Several object ranges read back to back as one stream.

use super::ObjectStore;
use crate::model::Reader;
use anyhow::Result;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// A byte range of one object, pinned to its ETag.
#[derive(Clone, Debug)]
pub struct ObjectRange {
    pub key: String,
    pub etag: String,
    /// Inclusive byte range; `None` reads the whole object.
    pub range: Option<(u64, u64)>,
}

/// Reads `ranges` in order as one stream. Each object is opened only when
/// the previous one is exhausted, so at most one download is open.
pub fn read_chain(store: Arc<dyn ObjectStore>, ranges: Vec<ObjectRange>) -> Reader {
    Box::new(Chain {
        store,
        pending: ranges.into(),
        current: None,
        opening: None,
    })
}

type Opening = Pin<Box<dyn Future<Output = Result<Reader>> + Send>>;

struct Chain {
    store: Arc<dyn ObjectStore>,
    pending: VecDeque<ObjectRange>,
    current: Option<Reader>,
    opening: Option<Opening>,
}

impl AsyncRead for Chain {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        loop {
            if let Some(reader) = this.current.as_mut() {
                let before = buf.filled().len();
                ready!(Pin::new(reader).poll_read(cx, buf))?;
                if buf.filled().len() > before || buf.remaining() == 0 {
                    return Poll::Ready(Ok(()));
                }
                this.current = None;
            } else if let Some(opening) = this.opening.as_mut() {
                let opened = ready!(opening.as_mut().poll(cx));
                this.opening = None;
                this.current = Some(opened.map_err(io::Error::other)?);
            } else if let Some(next) = this.pending.pop_front() {
                let store = this.store.clone();
                this.opening = Some(Box::pin(async move {
                    store.get(&next.key, Some(&next.etag), next.range).await
                }));
            } else {
                return Poll::Ready(Ok(()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MetadataMap;
    use crate::store::read_small;
    use crate::testing::MemoryStore;
    use bytes::Bytes;

    #[tokio::test]
    async fn reads_ranges_in_order_and_opens_objects_lazily() {
        let store = Arc::new(MemoryStore::default());
        let mut ranges = Vec::new();
        for (key, body) in [("a", "0123"), ("b", "4567"), ("c", "89")] {
            store
                .put(key, Bytes::from(body), &MetadataMap::new())
                .await
                .unwrap();
            let head = store.head(key).await.unwrap().unwrap();
            ranges.push(ObjectRange {
                key: key.into(),
                etag: head.etag,
                range: None,
            });
        }
        ranges[2].range = Some((0, 0));
        store.events.lock().unwrap().clear();

        let mut reader = read_chain(store.clone(), ranges.clone());
        let mut first = [0u8; 3];
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut first)
            .await
            .unwrap();
        assert_eq!(&first, b"012");
        assert_eq!(*store.events.lock().unwrap(), ["GET a"]);
        let rest = read_small(reader, 100).await.unwrap();
        assert_eq!(rest, b"345678");

        ranges[1].etag = "changed".into();
        assert!(read_small(read_chain(store, ranges), 100).await.is_err());
    }
}
