# Design

## Scope

In scope:

* Back up an existing ZFS filesystem snapshot as a raw full or incremental stream, with an automatically
  selected base or `--force-full-snapshot`.
* Encrypt each backup with its own random key, wrapped by a GPG public key.
* Stream to S3-compatible storage with bounded memory and no plaintext on disk (ciphertext parts may optionally be spooled to a temp file), for streams up to about 4 PiB.
* Prevent cooperating writers from overwriting each other through a per-backup lock.
* Restore the required chain into a new or matching existing filesystem, or export one stream to stdout.

Out of scope: creating, scheduling or pruning snapshots; retention and periodic full backups; checking
whole remote chains at backup time; forced rollback; lock stealing; resuming an interrupted upload;
zvols and other filesystems; loading native ZFS keys or mounting restored filesystems.

Terms: a *backup* is the object group under one snapshot prefix ([storage.md](storage.md)). It is
*committed* once `stream.encrypted` is published. Its *chain* follows incremental bases back to a full
backup or a matching local snapshot.

## Architecture

| Module (`src/`) | Responsibility |
| --- | --- |
| `cli.rs`, `main.rs` | Argument parsing, wiring, signal cancellation |
| `model.rs` | Names, object layout, `StreamIndex` / `BackupMetadata` |
| `backup/` | Backup job and failure recovery (`mod.rs`), base selection (`selection.rs`), upload pipeline (`pipeline.rs`) |
| `restore/` | Planning (`prepare.rs`), authentication (`verify.rs`), replay (`mod.rs`) |
| `store/` | `ObjectStore` trait, writer lock, metadata probe, bounded multipart upload, chained object reads |
| `s3/` | `ObjectStore` over HTTP: SigV4, credentials, retries, throughput guard |
| `crypto/` | Streaming AEAD, GPG key wrapping, key checksum |
| `zfs/` | `Zfs` trait and its `zfs`/`zpool` command implementation |
| `process.rs`, `rate.rs` | Bounded child-process I/O; ciphertext rate limiting |

`backup` and `restore` depend only on the `ObjectStore` and `Zfs` traits. Unit and workflow tests use in-memory fakes
(`src/testing.rs`). The binary uses `HttpStore` and `SystemZfs`.

### Data path and resource bounds

Backup: `zfs send -w` → encryption task → 2 MiB pipe → rate limiter → 64 KiB pipe → part (memory or `--part-temp-file`) →
`UploadPart`. Restore: chained `GET`s (one open object at a time) → decryption task → 2 MiB pipe →
`zfs receive -u` or stdout. Memory is thus at most one part plus fixed pipes and crypto segments,
whatever the stream size. Small objects, logs and command output are read with explicit caps. Plaintext,
keys and metadata never touch application-managed files. Swap and core dumps are not controlled.

A stream that is larger than one object continues in further objects (see
[storage.md](storage.md#stream-objects)). With AWS limits and the default 512 MiB maximum part size, each object
holds 5000 GiB, so 1 PB needs about 190 objects.

### ZFS command contract

`zfs get`, `zfs list`, `zpool get` and `zpool list` are always run with `-j -p` (OpenZFS 2.3+), never
with `--json-int` or a fallback to table output. Only the needed fields are decoded. `datasets` and
`pools` map keys must equal each entry's `name`. Types must be `FILESYSTEM`, `SNAPSHOT` or `POOL`
(`VOLUME` is rejected). Property values are raw strings, and GUIDs are validated as exact decimal
`u64` values. Missing or malformed data and command or permission failures are errors. A failed query
never counts as proof that something is absent.

`zfs diff -H` and `zfs send -nP` have their own machine-readable formats (they do not support `-j`).
Mutating commands are checked by exit status. Test harnesses can point `SNAPSHOT_TO_S3_ZFS_BIN` and
`SNAPSHOT_TO_S3_ZPOOL_BIN` at fake commands.

## Errors

* Backup succeeds only if `zfs send`, encryption and every upload succeed and the commit is confirmed.
  A failure to delete the lock after commit is reported separately. Metadata damage on an otherwise
  confirmed commit is a repairable failure: retain objects and log, release the lock, exit nonzero.
* Restore succeeds only if every replayed stream is fully authenticated and `zfs receive` succeeds.
  An authentication failure is never treated as end of stream.
* Unknown outcomes (an upload whose initiation or completion may have been applied) are reported as
  such and keep the lock. Nothing is retried blindly.

## Test coverage

| Scenario | Tests |
| --- | --- |
| Full and incremental recovery, data and GUIDs | `tests/e2e/run.sh`; `tests/workflow.rs` |
| Multi-object streams | `tests/workflow.rs::multi_object_stream_round_trip`; `store::multipart` and `store::chain` tests; E2E with `--max-object-size` |
| Incomplete remote chain | E2E (deleted `s1`); `restore::prepare::local_declared_base_needs_no_remote_parent` |
| Changed target or failing `zfs diff` stops before downloads | `restore::prepare::{dirty_target_stops_before_any_verification_download, diff_command_failure_stops_before_verification}`; E2E |
| No matching local base; wrong parent, metadata mismatch or cycle | `restore::prepare` tests; `tests/workflow.rs` tampering scenario |
| Corruption or truncation after a valid prefix | `tests/crypto_stream.rs`; `tests/workflow.rs::corrupt_stream_tail_stops_export` |
| Send, encryption, part, log or continuation failure | `tests/workflow.rs` failure scenarios; `store::multipart` tests |
| Concurrent writers, existing content | `store::lock` tests; `tests/live_http.rs` (opt-in, real S3) |
| Provider drops metadata or adds extra fields | `store::probe` tests; `tests/http_store/operations.rs`; `tests/workflow.rs` metadata-loss scenarios |
| Unknown completion, lost completion response | `tests/workflow.rs`; `store::multipart::completion_response_loss_requires_matching_object` |
| Receive failure mid-chain | `tests/workflow.rs::incremental_chain_stops_at_receive_failure_and_tampering` |
| Bounded memory, retries, service limits | `store::multipart::generated_large_stream_has_fixed_buffer_budget`; `tests/http_store/`; `tests/rate_limit.rs` |
| Tink interoperability | `tests/crypto_stream.rs` (official runtime check is opt-in) |

These tests use SeaweedFS and OpenZFS. Other providers (AWS S3, MinIO, B2, …) are untested until someone
runs `tests/live_http.rs` and the E2E script against them.
