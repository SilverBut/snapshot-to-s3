# Storage layout

Each backup creates a file at prefix `s3://${prefix}/{dataset_name}/{snapshot_name}/`. 

Files under this prefix are:

- `key.gpg` - Backup encryption key (GPG-encrypted)
- `key.sha256sum` - SHA256 sum for decrypted content of `key.gpg`
- `meta.json.encrypted` - Backup metadata
- `stream.encrypted` - Snapshot data
- `backup.log.encrypted` - Backup log

`key.gpg` contains the per-backup encryption key (we will call it `key`) and encrypted by given GPG key. 
`key.sha256sum` is hash checksum for `key`.

All other `*.encrypted` files are encrypted by `key` using [AES-GCM-HKDF][aes-gcm-hkdf], or `AES128_GCM_HKDF_1MB` in
a more precision way to describe it. This allows ~2000TiB encryption per file, which is usually enough. 

Associated data of the stream should be the SHA256 checksum value of `meta.json`. Other files have no associated data.

The following metadata is stored for `stream.encrypted` (using the configured metadata prefix, default `x-amz-meta`):

- `x-amz-meta-gpg-key-id` - GPG key identifier used for encryption
- `x-amz-meta-fs-type` - File system type. Now fixed at `zfs`
- `x-amz-meta-vol-id` - Volume GUID of the snapshot's parent filesystem
- `x-amz-meta-current-snapshot-id` - Current snapshot GUID
- `x-amz-meta-base-snapshot-id` - Base snapshot GUID, if this is an incremental backup

These metadata enables automatic incremental backup detection and chain validation.

For example, with this given dataset:

```bash
$ zfs list -t all -o name,type,guid
NAME                                  TYPE                        GUID
vp1                                   filesystem  17365166662743153220
vp1/guid_lab                          filesystem  14148175508811798995
vp1/guid_lab/alpha                    filesystem  14066916649205506663
vp1/guid_lab/alpha@s1                 snapshot    16158825896409765662
vp1/guid_lab/alpha@s2                 snapshot    12631906673493169747
vp1/guid_lab/alpha@s3                 snapshot    10890719445259130145
```

Then if you execute this command:

```bash
snapshot-to-s3 backup zfs:vp1/guid_lab/alpha@s1  s3://my-backup-bucket/backups/dataset-snapshot --gpg-key-id ED7C55E60B7543A2
```

The resulting file would be under:

```bash
s3://my-backup-bucket/backups/dataset-snapshot/vp1/guid_lab/alpha/s1/
```

And it would have these metadata for `stream.encrypted`:

- `x-amz-meta-gpg-key-id`: `4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2`. Note here ID is expanded to full format.
- `x-amz-meta-vol-id`: `14066916649205506663`
- `x-amz-meta-current-snapshot-id`: `16158825896409765662`
- `x-amz-meta-base-snapshot-id`: empty

If snapshot `s2` is also uploaded and if `s1` is selected as base, the new file would be under:

```bash
s3://my-backup-bucket/backups/dataset-snapshot/vp1/guid_lab/alpha/s2/
```

With these metadata for `stream.encrypted`:

- `x-amz-meta-gpg-key-id`: `4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2`. Note here ID is expanded to full format.
- `x-amz-meta-vol-id`: `14066916649205506663`
- `x-amz-meta-current-snapshot-id`: `12631906673493169747`
- `x-amz-meta-base-snapshot-id`: `16158825896409765662`

---

[aes-gcm-hkdf]: https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming?hl=zh-cn