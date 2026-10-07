# Design of program

## Requirements and boundaries

This document and the linked storage and workflow documents specify the intended behavior, not the implementation
status of each feature.

* Back up an existing, user-selected ZFS snapshot using a raw full or incremental send.
* Select an incremental base automatically, or send a full backup with `--force-full-snapshot`.
* Generate an independent random key for each backup and protect it with the selected GPG public key.
* Stream encrypted data to S3-compatible storage without application-managed plaintext temporary files.
* Prevent participating writers from overwriting backups by following the same per-backup lock protocol.
* Restore the required backup chain to a new target, or continue from a matching latest snapshot on an existing target.
* Export only the selected backup's send stream to stdout, without replaying its dependencies.

A *backup* is the group of objects under one snapshot prefix. Its *source chain* follows incremental bases back to a
full backup or a usable local base. A *committed backup* has completed the publication protocol in
[storage.md](storage.md).

Snapshot creation, scheduling, retention, periodic full backups, dependency preservation and backup inspection belong
to external management. Backup does not prove that the remote chain can recover an empty dataset. Users also manage
native ZFS keys, mounting and application-level checks after receiving data.

Automatic forced rollback, chain-length policies, lock stealing, cross-process upload resumption, name migration and
additional filesystem support are not requirements of this design. Restore does not load native ZFS keys or require
mounting the result. Raw send preserves native encryption when present; it does not add native encryption to an
unencrypted dataset.

## Component layout

Note, directories mentioned in this paragraph are relative to the source root.

`fs/` provides compatible layer for different file system. Each struct should implement a set of traits which can:

* List volumes (for `zfs` this means all vol and subvols)
* List snapshots for a volume
* For a volume or snapshot, get its properties and its ID
* Get raw stream of a snapshot, or a diff of two snapshots
* Receive a stream and report the command's result
* Check an existing filesystem against its latest snapshot during restore preparation

Trait inclues `SnapshotableFilesystem`, `Volume`, `Snapshot`. For now, we need two struct `dummy` and `zfs`.

`storage/` mainly wraps client to access S3-compatible object storage to provide those capabilities:

* List files in bucket witha optional prefix
* Stat a file info
* Get a file's user defined metadata (like `x-amz-meta-*` or `x-cos-meta-`, depends on user config)
* Upload a file
* Read an object as a stream
* Upload stream parts, complete or abort a multipart upload
* Create a lock object atomically if absent, and delete a held lock

Snapshot data is uploaded directly as an encrypted stream, without a tar container. Multipart buffering, publication
and failure handling follow [storage.md](storage.md).

`crypto/` is where out encryption reload code resides. It can:

* Genearte a random key
* For a given key, create a encrytion interface which accepts a stream input and send encrypted stream out
* Decrypt and authenticate each encrypted object's stream through its final segment
* GPG related
    * Find (public) encyrpt-capable key indicated by user id or key id
    * Encrypt with the key
    * Decrypt the backup key using an available private key

Plaintext backup keys, metadata and stream data must remain in memory or pipes, not application-managed temporary
files. This does not claim to control operating-system facilities such as swap or core dumps.

`utils/` is a set of tools. `utils/mbuffer.rs` provides [mbuffer][mbuffer] ability so we can apply speed limit.

## Resource and error handling

Use bounded buffers and backpressure for data, logs, upload concurrency and retry queues; memory must not grow with
the total stream size. Retry retained ciphertext bytes rather than re-encrypting a consumed plaintext stream.
Multipart sizing must respect the configured service's part and object limits.

Backup succeeds only when all producers and uploads succeed and the stream object is confirmed committed. Failure
to remove the lock after commit is reported separately from failure to publish the backup.

Restore succeeds only when all required streams finish authenticated decryption and `zfs receive` succeeds.
Preparation and prefix verification do not guarantee that subsequent receive will succeed. No default second,
full-stream verification pass is required. Authentication errors must never be treated as normal end-of-stream.
Diagnostics and final results must be explicit; stdout export sends diagnostics to stderr.

See [workflow.md](workflow.md) for the ordering of preparation, verification and replay, including early rejection by
`zfs diff` and reporting partially completed chains.

## Acceptance scenarios

These are requirements for later implementation validation, not claims that tests already exist.

| Scenario | Required outcome |
| --- | --- |
| Full and multi-step incremental recovery | Receive the required snapshots with matching GUIDs and data |
| Incomplete remote chain | Warn; allow recovery from a matching local base, reject recovery to an empty target |
| Existing target has changes, or `zfs diff` fails | Stop during Prepare, before downloading keys, metadata or stream data for Verification |
| No matching latest local base | Reject without forced rollback |
| Wrong parent GUID, metadata mismatch or dependency cycle | Report an error without proceeding with an invalid chain |
| Valid prefix followed by corruption or truncation | Report decryption or receive failure, never success |
| Send, encryption, part or log upload failure | Do not commit the stream; report cleanup and residual objects |
| Concurrent writers or existing backup content | Only the lock holder may write; existing content is not overwritten |
| Unknown completion result or stale lock | Report the state explicitly; do not blindly retry or steal the lock |
| Receive fails after earlier chain steps | Stop and identify the last successfully received snapshot |
| Stdout export | Export one stream, keep diagnostics separate, fail explicitly if the export is incomplete |
| Small and large streams, retries and service limits | Preserve ciphertext, keep memory bounded and report exceeded limits |

---

[mbuffer]: https://www.maier-komor.de/mbuffer.html