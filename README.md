# snapshot-to-s3

Encrypt snapshot of your modern file system and upload to S3-compatible object storage.

## Features

* Stream processing to prevent additional disk usage and risk of plaintext leak
* ZFS on Linux currently supported (more can be easily added later)
* Single backup file writes to a S3-compatible storage
* Each uploaded backup is encrypted with AES-256-GCM with a new key
* Use your favorite GPG key as KEK (key-encryption-key)
* Auto detect incremental backup

## Installation

Build from source:

```bash
cargo build --release
```

The binary will be available at `target/release/snapshot-to-s3`.

## Usage

### Backup a Snapshot

```bash
snapshot-to-s3 backup \
  --filesystem zfs \
  --snapshot pool/dataset@snapshot-name \
  --bucket my-backup-bucket \
  --gpg-key "user@example.com" \
  --gpg-key-file /path/to/public-key.asc \
  --rate-limit 10485760  # Optional: 10 MB/s
```

### List Volumes

```bash
snapshot-to-s3 list-volumes --filesystem zfs
```

### List Snapshots

```bash
snapshot-to-s3 list-snapshots --filesystem zfs --volume pool/dataset
```

## Configuration

### AWS Credentials

The tool uses the AWS SDK for Rust, which automatically loads credentials from:
- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- AWS credentials file (`~/.aws/credentials`)
- IAM role (when running on EC2)

### GPG Key Setup

You need a GPG public key to encrypt the backup encryption keys. You can either:
1. Use a key from your GPG keyring (not yet implemented - use `--gpg-key-file`)
2. Provide a key file with `--gpg-key-file`

Generate a GPG key pair if you don't have one:

```bash
gpg --full-generate-key
gpg --export -a "user@example.com" > public-key.asc
```

## Storage Layout

Each backup creates a single tar file at `s3://bucket/volume_id/snapshot_id/backup.tar` containing:

- `key.gpg`: The AES-256-GCM encryption key, encrypted with your GPG public key
- `meta.json.encrypted`: Backup metadata (volume ID, snapshot ID, mode, parent info)
- `stream.encrypted`: The actual snapshot data stream
- `log.encrypted`: Backup log

User-defined metadata is also stored with the S3 object:
- `x-amz-meta-gpg-id`: GPG key identifier
- `x-amz-meta-mode`: `full` or `incremental`
- `x-amz-meta-parent-vol-id`: Parent volume ID (for incremental)
- `x-amz-meta-parent-snap-id`: Parent snapshot ID (for incremental)

## Architecture

The codebase is organized into modules:

- `fs/`: Filesystem abstraction layer with support for ZFS and dummy filesystem
- `crypto/`: Encryption (AES-256-GCM) and GPG key management
- `storage/`: S3 client wrapper and streaming tar upload
- `utils/`: Utilities like rate-limited buffering
- `workflow/`: Backup workflow orchestration

See `docs/design.md` for detailed design documentation.

## Development

Run tests:

```bash
cargo test
```

Run with dummy filesystem for testing:

```bash
cargo run -- list-volumes --filesystem dummy
```

## License

AGPL-3.0 - See LICENSE file for details.