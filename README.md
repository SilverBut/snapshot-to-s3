# snapshot-to-s3

Stream encrypted ZFS filesystem snapshots to S3-compatible object storage, and restore them.

* Raw `zfs send -w` full or incremental streams; the incremental base is chosen automatically.
* Streaming AEAD (Tink-compatible `AES128_GCM_HKDF_1MB`) with a fresh key per backup, wrapped by GPG.
* Plaintext and keys stay in memory and pipes and never touch disk; memory does not grow with the
  stream size. Ciphertext parts can optionally be spooled to a temp file (`--part-temp-file`). One backup may be up to about 4 PiB (see [Limits](#limits-and-resource-use)).
* An atomic per-backup lock; existing or partial backups are never overwritten.
* Restore authenticates every required backup before replaying the chain into `zfs receive -u`, or
  exports one stream to stdout.
* Own S3 client (SigV4, no AWS SDK) for AWS and S3-compatible services.

Only ZFS filesystems are supported; volumes (zvols) are rejected. Snapshot creation, scheduling,
retention, periodic full backups and backup inspection are left to other tools.

## Requirements

* Linux with OpenZFS 2.3+ (`zfs`/`zpool` with JSON output, `-j`)
* GnuPG (`gpg`)
* Rust stable to build: `cargo build --release --locked`

## Usage

### Backup

```bash
snapshot-to-s3 backup zfs:pool/dataset@snap s3://bucket/backups --gpg-key-id user@example.com
```

The backup is written under `s3://bucket/backups/pool/dataset/snap/`. The snapshot must already exist.

| Option | Meaning |
| --- | --- |
| `--gpg-key-id` (or `GPG_KEY_ID`) | Selector resolving to exactly one encryption-capable public key |
| `--force-full-snapshot` | Send a full stream instead of choosing an incremental base |
| `--probe-metadata-multipart` | Also check metadata with a tiny multipart upload before backup (off by default; extra S3 requests). The simple PUT metadata check always runs |
| `--rate-limit BYTES_PER_SEC` | Limit ciphertext throughput |
| `--force-overwrite` | Delete any existing backup objects under the destination prefix and write anew. Destructive; the exclusive writer lock is still required and never bypassed |
| `--progress [tty\|SECONDS\|off]` | Progress on stderr (size, elapsed time, speed). `tty` is a live line, a number prints one line per that many seconds; default and bare flag use `tty` on a terminal, else every 10 s. Also accepted by `restore`; never writes to stdout |
| `--max-part-size` | Largest part (default 512 MiB). Also the memory, or temp-file, held for the part being uploaded; must not exceed the provider limit (5 GiB on AWS) |
| `--part-temp-file PATH` | Hold each ciphertext part in this file instead of memory, so large parts fit small-memory hosts. The path must not exist; it is created with mode 0600 and removed afterwards |
| `--min-part-size` (default 100 MiB), `--max-parts`, `--max-object-size` | Multipart limits (maximums default to AWS S3); lower `--min-part-size` for services that allow smaller parts |

Diagnostics: set `RUST_LOG` (for example `RUST_LOG=debug`) for logs of external commands, S3 requests and
upload steps. Logs go to stderr only (default level `warn`), so `restore ... stdout:` output stays clean.

An incremental base must be an older local snapshot whose committed backup exists at the same prefix.
Choosing a base does not check that the base's own chain is complete; keep the backups a chain needs
when applying lifecycle rules.

### Restore

```bash
# Replay the required chain into pool/dataset (or into --target-pool / --target-dataset).
snapshot-to-s3 restore s3://bucket/backups zfs:pool/dataset@snap --target-dataset restored/dataset

# Write only this backup's decrypted send stream to stdout.
snapshot-to-s3 restore s3://bucket/backups stdout:pool/dataset@snap > snap.zstream
```

Name the snapshot as it was backed up. `--target-dataset` is relative to `--target-pool`, which defaults
to the source pool. `--gpg-key-id` optionally requires a specific recipient for the selected backup.

For an existing target, its latest snapshot must be in the chain and the target must be unchanged since
then (`zfs diff`); restore never rolls back. Restore stops at the first failure and reports the last
received snapshot. It does not load native ZFS keys or mount the result. A stdout export may have written
authenticated data before a failure; the exit status is then nonzero.

### Exit status

`0` success, `1` any failure (details on stderr), `2` invalid command line. SIGINT and SIGTERM cancel
cleanly through the same failure handling.

A stream published with matching size but missing or wrong metadata exits `1` while retaining the
uploaded backup and log and releasing its lock. The log contains expected metadata; stderr prints
the expected headers for manual repair. See [storage.md](docs/storage.md#lock-and-commit-protocol).

## Configuration

### S3 endpoint

| Option | Meaning |
| --- | --- |
| `--endpoint URL` | S3-compatible endpoint; implies path-style addressing |
| `--region` (or `AWS_REGION`, then `AWS_DEFAULT_REGION`) | Signing region, default `us-east-1` |
| `--path-style` / `--virtual-hosted-style` | Override the addressing style |
| `--metadata-prefix` | User-metadata header prefix, default `x-amz-meta` (e.g. `x-bz-info` for B2) |
| `--signing-service` | SigV4 service name, default `s3` |

HTTP liveness and retry behavior can be tuned with `SNAPSHOT_TO_S3_HTTP_*` variables; see
[storage.md](docs/storage.md#http-transfers).

### Credentials

In this order: `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` (optional `AWS_SESSION_TOKEN`); the shared
credentials file (`AWS_SHARED_CREDENTIALS_FILE` or `~/.aws/credentials`, profile `AWS_PROFILE`).
Missing credentials fail immediately when the S3 client is created. There are no credential flags
or automatic credential refresh. Other provider-chain sources (SSO, ECS, web identity, …) are not supported.

### GPG

Backup needs only the recipient's public key; restore needs the private key. Keeping them in separate
keyrings is recommended. A usable key shows `[E]` in `gpg --list-keys --with-subkey-fingerprint KEY`.
The selector is resolved to a full fingerprint, which is stored with the backup.

### Bucket permissions

`s3:PutObject` (objects and, unless locking is disabled, the conditional lock), `s3:GetObject`,
`s3:ListBucket`, `s3:AbortMultipartUpload`, and `s3:DeleteObject` for lock and probe objects
(`*/.lock`, `*/.metadata-probe`, `*/.metadata-probe-multipart`).
Removing partial backups is a manual operator task.

When locking is enabled, the service must support conditional create on PUT and preserve user metadata.
By default, locks use `If-None-Match: *`; for Tencent COS,
`--lock-detection-mode x-cos-forbid-overwrite` selects COS's `x-cos-forbid-overwrite: true` header
(not effective on versioning-enabled buckets). Lock acquisition checks the selected condition.
Metadata probing happens afterwards, inside the backup prefix next to `.lock`; see
[storage.md](docs/storage.md#lock-and-commit-protocol). A lifecycle rule that aborts incomplete multipart
uploads is recommended.

`--lock-detection-mode dangerously-skip` disables the lock and its condition verification; it is unsafe if
another writer can back up the same prefix concurrently. Metadata probing and the empty-prefix check
still run.

## Limits and resource use

* **Memory** is bounded and independent of the stream size. Backup holds one part of at most
  `--max-part-size` (in memory, or on disk with `--part-temp-file`) plus 2 MiB and 64 KiB pipes; restore holds a 2 MiB pipe and 1 MiB segments.
  Command output and small objects are read with fixed caps.
* **Size**: a stream larger than one object continues in further objects of at most `--max-parts` ×
  part size bytes (512 MiB × 10,000 = 5000 GiB with the defaults). Parts are sized from the send estimate
  plus 25% headroom; if that exceeds one object, a warning suggests a larger `--max-part-size`. Up to 1,000,000 objects per backup are allowed; the encryption format allows about
  4 PiB per backup.
* GET downloads resume from the current offset after transient failures; uploads retry identical part
  bytes. Retries are per object.

## Documentation

* [Design](docs/design.md): scope, architecture, resource bounds and test coverage
* [Storage](docs/storage.md): object layout, encryption, lock and commit protocol, HTTP behavior
* [Workflow](docs/workflow.md): backup and restore step by step
* [Engineering practices](docs/engineering.md): safe refactoring, test integrity, mutation testing
* [Contributing](CONTRIBUTING.md) and [development environment](DEVELOPMENT.md)

## License

AGPL-3.0; see [LICENSE](LICENSE).
