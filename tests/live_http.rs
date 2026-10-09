use anyhow::{Context, Result};
use bytes::Bytes;
use snapshot_to_s3::model::MetadataMap;
use snapshot_to_s3::s3::{HttpConfig, HttpStore};
use snapshot_to_s3::store::{ObjectStore, Part};
use tokio::io::AsyncReadExt;

#[tokio::test]
#[ignore = "requires a dedicated local S3 endpoint and test credentials"]
async fn real_s3_conditions_ranges_and_multipart() -> Result<()> {
    let endpoint = std::env::var("TEST_S3_ENDPOINT").context("set TEST_S3_ENDPOINT")?;
    let bucket = std::env::var("TEST_S3_BUCKET").context("set TEST_S3_BUCKET")?;
    let store = HttpStore::new(HttpConfig {
        bucket,
        endpoint: Some(endpoint),
        region: "us-east-1".into(),
        metadata_prefix: "x-amz-meta".into(),
        signing_service: "s3".into(),
        path_style: true,
    })
    .await?;
    let prefix = format!("live-http-{}/", hex::encode(rand::random::<[u8; 16]>()));
    let key = format!("{prefix}conditional");
    let multipart = format!("{prefix}stream");
    let abort_key = format!("{prefix}abort");
    let result: Result<()> = async {
        let (a, b) = tokio::join!(
            store.put_if_absent(&key, Bytes::from_static(b"winner-a")),
            store.put_if_absent(&key, Bytes::from_static(b"winner-b")),
        );
        assert_ne!(a?, b?);
        assert!(
            !store
                .put_if_absent(&key, Bytes::from_static(b"overwrite"))
                .await?
        );
        let index = MetadataMap::from([("test-id".into(), "round-trip".into())]);
        let upload = store.create_upload(&multipart, &index).await?;
        let bytes = Bytes::from(vec![0xA5; 5 * 1024 * 1024]);
        let a = store
            .upload_part(&multipart, &upload, 1, bytes.clone())
            .await?;
        let b = store
            .upload_part(&multipart, &upload, 2, Bytes::from_static(b"tail"))
            .await?;
        store
            .complete_upload(
                &multipart,
                &upload,
                &[Part { number: 1, etag: a }, Part { number: 2, etag: b }],
            )
            .await?;
        let head = store
            .head(&multipart)
            .await?
            .context("missing committed object")?;
        assert_eq!(head.size, bytes.len() as u64 + 4);
        assert_eq!(head.metadata, index);
        let mut reader = store
            .get(
                &multipart,
                Some(&head.etag),
                Some((head.size - 4, head.size - 1)),
            )
            .await?;
        let mut range = Vec::new();
        reader.read_to_end(&mut range).await?;
        assert_eq!(range, b"tail");
        let upload = store.create_upload(&abort_key, &MetadataMap::new()).await?;
        store.upload_part(&abort_key, &upload, 1, bytes).await?;
        store.abort_upload(&abort_key, &upload).await?;
        assert!(store.head(&abort_key).await?.is_none());
        let listed = store.list(&prefix).await?;
        assert!(listed.contains(&key));
        assert!(listed.contains(&multipart));
        Ok(())
    }
    .await;
    for key in store.list(&prefix).await? {
        store
            .delete(&key)
            .await
            .with_context(|| format!("cleanup {key}"))?;
    }
    result
}
