# snapshot-to-s3

Encrypt ZFS filesystem snapshots and upload to S3-compatible object storage.

This README and the documents in `docs/` describe the target requirements and design, not a claim that every behavior
has already been implemented.

## Features

* Stream-first pipeline: plaintext remains in memory/pipes, not app-managed temporary files
* Linux ZFS filesystem snapshots (`zfs:dataset@snapshot`) with raw send/receive
* Explicit rejection of non-filesystem datasets (for example zvol/block volumes)
* Streaming AEAD encryption (`AES128_GCM_HKDF_1MB`) with per-backup wrapped keys
* Backup-chain metadata, lock protocol and authenticated restore/stdout export workflow
* S3-compatible HTTP + SigV4 implementation with configurable endpoint, metadata prefix and signing service

## Usage

Ensure you have:

* GPG CLI
* ZFS CLI

Then install the program by build from source:

```bash
cargo build --release --locked
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

To skip base selection and force a full backup (`force-full` mode):

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com" \
  --force-full-snapshot
```

Each backup is a group of separate objects, not a tar archive. Writers acquire an atomic per-snapshot `.lock` and
refuse to overwrite committed or partial backup content. Snapshot scheduling, retention, periodic full backups and
backup inspection are external responsibilities. Selecting an incremental base does not prove that its entire remote
chain is intact; preserve required dependencies when applying external lifecycle rules.

With ciphertext rate limiting:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-backup-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com" \
  --rate-limit 10485760  # 10 MB/s
```

For S3-compatible services (for example MinIO or Backblaze B2), set endpoint, metadata prefix, region, and optional addressing/signing overrides:

```bash
snapshot-to-s3 backup \
  zfs:pool/dataset@snapshot-name \
  s3://my-bucket/backups/dataset-snapshot \
  --gpg-key-id "user@example.com" \
  --endpoint https://s3.us-west-002.backblazeb2.com \
  --region us-west-002 \
  --metadata-prefix x-bz-info \
  --signing-service s3 \
  --virtual-hosted-style
```

Addressing defaults to **path-style** when `--endpoint` is set (unless `--virtual-hosted-style` is provided), and to virtual-hosted style for default AWS endpoints. You may force path-style with `--path-style`.

Reusing a snapshot name at the same backup prefix is refused if backup content already exists. Failed uploads may
leave partial objects or a lock; confirm the writer and upload are stopped before manually cleaning them.

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
  --target-dataset relative/dataset/path
```

`--target-dataset` is interpreted relative to `--target-pool`. It must not include another pool segment.

For ZFS restore, follow the parent object keys and replay only the required chain, stopping at a matching latest local
snapshot or a full backup. An incomplete remote chain cannot recover an empty target, but may still work with a matching
local base. Use the original backup names to locate the source, even when restoring into a different target.

For an existing target filesystem, Prepare confirms the latest snapshot as the receive base and runs `zfs diff` against
the current filesystem. Changes or a failed check stop recovery before key, metadata or stream verification downloads.
Prevent concurrent writes throughout restore; this early check cannot guarantee every subsequent receive will succeed.
The tool does not automatically force rollback.

Successful restore means complete authenticated decryption and successful ZFS receive. It does not require loading
native ZFS keys or mounting the result. Users manage these separately; raw send retains native encryption only when
the source already has it. Chain replay can stop after earlier snapshots were received if a later step fails.

For debug purpose you may don't want to write to the pool directly, then you can send decrypted backup stream to stdout:

```bash
snapshot-to-s3 restore \
  s3://my-bucket/backups/dataset-snapshot \
  stdout:pool/dataset@snapshot-name \
  --gpg-key-id "user@example.com"
```

Stdout exports only this backup's send stream, including a single incremental stream when applicable. It does not
concatenate dependencies or inspect a local target. Diagnostics go to stderr; a nonzero exit status may follow partial
output if decryption or writing fails.

## Multipart and memory controls

Backup upload limits are explicit CLI controls:

* `--min-part-size` (default 5 MiB)
* `--max-part-size` (default 5 GiB)
* `--max-parts` (default 10,000)
* `--max-object-size` (default 5 TiB)
* `--part-buffer-size` (default 64 MiB)

`--part-buffer-size` caps in-process part buffering for the single active multipart stream. Effective memory also includes
crypto/rate pipes and runtime overhead; it is bounded and does not grow with total stream size. Large objects near a
service's maximum may require a larger explicit buffer and limits aligned to that service's part/object constraints.

## Configuration

### S3 Credentials

The binary does **not** use the AWS Rust SDK provider chain. It implements the currently supported credential sources:

- Environment variables: `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` (optional `AWS_SESSION_TOKEN`)
- Shared credentials file (`~/.aws/credentials` or `AWS_SHARED_CREDENTIALS_FILE`) with `AWS_PROFILE`
- EC2 IMDSv2 role credentials

Command-line credential flags are intentionally unsupported to reduce secret leakage risk.

Other AWS provider-chain sources (for example SSO, ECS task role, EKS IRSA, web identity, process providers) are not
currently claimed unless separately implemented and validated.

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
- `s3:AbortMultipartUpload` - Abort incomplete multipart uploads
- `s3:DeleteObject` - Release lock objects

`s3:PutObject` covers multipart initiation, part upload, completion, user-defined metadata and conditional creation of
the lock. User-defined metadata is not object tagging. The endpoint must support atomic create-if-absent for locks;
ordinary HEAD followed by PUT is insufficient.

Before publication, backup performs atomic-condition and metadata capability probes under:
`{configured-s3-prefix}/.snapshot-to-s3-probes/<random>/.lock`.
The HTTP layer enforces a `.lock` probe-key suffix.

Endpoints that ignore `If-None-Match: *` or do not preserve the configured metadata-header prefix are rejected early.
Probe lock keys are transient and are deleted after verification.

Configure bucket and object permissions in your AWS IAM policy. Restrict the application's `s3:DeleteObject` permission
to lock-suffix objects (including transient probe locks); manual cleanup of partial backups is a separate operator action.

## Internals

How backup files organized? How the best incremental base is selected? Why ... ?

For detailed design information, see:

* [Requirements and component design](docs/design.md)
* [Storage format, locks and publication](docs/storage.md)
* [Backup and restore workflow](docs/workflow.md)

## License

AGPL-3.0 - See LICENSE file for details.

---

[aes-gcm-hkdf]: https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming?hl=zh-cn