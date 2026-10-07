# snapshot-to-s3

Encrypt snapshots from modern filesystems and upload to S3-compatible object storage.

## Features

* Stream processing to prevent additional disk usage and risk of plaintext leak
* ZFS on Linux currently supported
* Encrypted backups with [AES-GCM-HKDF][aes-gcm-hkdf]
* GPG-protected backup keys
* Automatic incremental backup detection

## Usage

Ensure you have:

* GPG CLI
* ZFS CLI

Then install the program by build from source:

```bash
cargo build --release
```

**Before** you run any command, ensure you have setup credentials. See next chapter about now to do this.

### Backup a Snapshot

Basic usage with AWS S3 bucket with a prefix:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com"
```

Backup is always collected in raw format from ZFS side. The tool will select a base snapshot automatically. See
document to understand how it works.

With rate limiting:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com" \
  --rate-limit 10485760  # 10 MB/s
```

For S3-compatible services (e.g., MinIO, Backblaze B2) you may want to change the endpoint and use another metadata prefix:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com" \
  --endpoint https://s3.us-west-002.backblazeb2.com \
  --region us-west-002 \
  --metadata-prefix x-bz-info
```

Duplicate snapshot name is not a wise idea.

### Restore a Snapshot

If you want to restore a snapshot, give **exactly** same S3 prefix and the backup source info:

```bash
snapshot-to-s3 restore \
  s3://my-bucket/backups/dataset-snapshot \
  zfs:pool/dataset@snapshot-name \
  --gpg-key-id "user@example.com"
```

Sometimes you may want to restore into another pool and/or dataset:

```bash
snapshot-to-s3 restore \
  s3://my-bucket/backups/dataset-snapshot \
  zfs:pool/dataset@snapshot-name \
  --gpg-key-id "user@example.com" \
  --target-pool optional_target_pool \
  --target-dataset optional_target_dataset
```

If source snapshot is a incremental backup, the program will automatically check and pull all related source snapshot
recursively. This feature can not be turned off because ZFS requires a linear-style snapshot history.

For debug purpose you may don't want to write to the pool directly, then you can send decrypted backup stream to stdout:

```bash
snapshot-to-s3 restore \
  s3://my-bucket/backups/dataset-snapshot \
  stdout:pool/dataset@snapshot-name \
  --gpg-key-id "user@example.com"
```

## Configuration

### S3 Credentials

The tool uses standard AWS credential loading:

- Environment variables: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
- AWS credentials file: `~/.aws/credentials`
- IAM role (when running on EC2)

Define credentials by options is not supported, to reduce risk of leaking credentials.

If you are using a S3-compatible service, just ensure they support S3-style auth.

### GPG Key Setup

Backup file is finally protected by a GPG key, so you need to have a valid GPG keybox with a key ID available for
encryption. It's better if you don't put private and public key together into same keybox.

To check if a key ID is available in keybox for encryption:

```bash
$ gpg --list-keys --with-subkey-fingerprint 4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2
pub   ed25519 2026-10-06 [SC]
      4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2
uid           [ultimate] user@example.com
sub   cv25519 2026-10-06 [E]
      D3E17C2F5895E363AD1EFEA3B113F539F9C5F1B3
```

The `[E]` mean a key is available for encryption.

Once you have generated the GPG key, you can refer the key in `--gpg-key-id` argument or `GPG_KEY_ID` environment variable.

### S3 Bucket Permissions

Your AWS/S3 account needs the following permissions for the backup bucket:

- `s3:PutObject` - Upload backup files
- `s3:GetObject` - Read existing backups (for incremental detection)
- `s3:ListBucket` - List existing backups
- `s3:PutObjectTagging` - Set metadata on backup objects
- `s3:CreateMultipartUpload` - Initiate multipart uploads
- `s3:UploadPart` - Upload parts in multipart uploads
- `s3:CompleteMultipartUpload` - Complete multipart uploads
- `s3:AbortMultipartUpload` - Abort incomplete multipart uploads

These permissions should be configured in your AWS IAM policy for the bucket.

## Internals

How backup files organized? How the best incremental base is selected? Why ... ?

For detailed design information, see `docs/`.

## License

AGPL-3.0 - See LICENSE file for details.

---

[aes-gcm-hkdf]: https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming?hl=zh-cn