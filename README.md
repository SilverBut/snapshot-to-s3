# snapshot-to-s3

Encrypt snapshots from modern filesystems and upload to S3-compatible object storage.

## Features

* Stream processing to prevent additional disk usage and risk of plaintext leak
* ZFS on Linux currently supported
* Encrypted backups with AES-256-GCM
* GPG-encrypted backup keys
* Automatic incremental backup detection

## Installation

Build from source:

```bash
cargo build --release
```

The binary will be available at `target/release/snapshot-to-s3`.

## Usage

### Backup a Snapshot

Basic usage with AWS S3:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key "user@example.com"
```

With rate limiting:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key "user@example.com" \
  --rate-limit 10485760  # 10 MB/s
```

For S3-compatible services (e.g., MinIO, Backblaze B2):

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-bucket/backups/dataset-snapshot \
  --gpg-key "user@example.com" \
  --endpoint https://s3.us-west-002.backblazeb2.com \
  --region us-west-002
```

## Configuration

### AWS Credentials

The tool uses standard AWS credential loading:
- Environment variables: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`
- AWS credentials file: `~/.aws/credentials`
- IAM role (when running on EC2)

For custom credentials, use command-line options:

```bash
snapshot-to-s3 backup ... \
  --access-key-id YOUR_ACCESS_KEY \
  --secret-access-key YOUR_SECRET_KEY \
  --region us-east-1
```

### GPG Key Setup

You need a GPG key pair to encrypt the backup encryption keys.

Generate a new GPG key:

```bash
gpg --full-generate-key
```

Follow the prompts to create a key. Use a strong passphrase to protect your private key.

Export your public key (optional, for sharing):

```bash
gpg --export -a "user@example.com" > my-public-key.asc
```

**Note:** GPG keyring lookup is currently not implemented. You'll need to ensure your GPG key is available in the system keyring.

### S3 Bucket Permissions

Your AWS/S3 account needs the following permissions for the backup bucket:

- `s3:PutObject` - Upload backup files
- `s3:GetObject` - Read existing backups (for incremental detection)
- `s3:ListBucket` - List existing backups
- `s3:PutObjectTagging` - Set metadata on backup objects

Example IAM policy:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:GetObject",
        "s3:ListBucket",
        "s3:PutObjectTagging"
      ],
      "Resource": [
        "arn:aws:s3:::my-backup-bucket/*",
        "arn:aws:s3:::my-backup-bucket"
      ]
    }
  ]
}
```

## Backup Storage

### File Layout

Each backup creates a file at: `s3://bucket/volume_id/snapshot_id/backup.tar`

The tar file contains:
- `key.gpg` - Backup encryption key (GPG-encrypted)
- `meta.json.encrypted` - Backup metadata
- `stream.encrypted` - Snapshot data
- `log.encrypted` - Backup log

### Object Metadata

The following metadata is stored with each backup object (using the configured metadata prefix, default `x-amz-meta`):

- `x-amz-meta-gpg-id` - GPG key identifier used for encryption
- `x-amz-meta-mode` - Backup mode: `full` or `incremental`
- `x-amz-meta-parent-vol-id` - Parent volume ID (for incremental backups)
- `x-amz-meta-parent-snap-id` - Parent snapshot ID (for incremental backups)

This metadata enables automatic incremental backup detection and chain validation.

### Incremental Backups

The tool automatically detects when an incremental backup is possible by:
1. Checking for existing backups of the same volume
2. Verifying the parent snapshot chain exists locally
3. Using incremental send when possible

For detailed design information, see `docs/design.md`.

## License

AGPL-3.0 - See LICENSE file for details.