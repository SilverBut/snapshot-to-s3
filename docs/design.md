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

Module names below are relative to `src/` and describe the current rebuild architecture target.

### Domain and protocol model

* `model.rs`: validated snapshot/dataset names, decimal GUID rules, S3 location and per-backup metadata/index
  structures.
* `selection.rs`: incremental-base discovery policy and candidate diagnostics.
* `prepare.rs`: restore Prepare planning, chain validation and local/remote preconditions.

### ZFS and process execution

* `zfs_api.rs`: focused ZFS interface used by backup/restore workflows.
* `zfs.rs`: Linux `zfs`/`zpool` command integration for filesystem snapshots only.
* `process.rs`: bounded process I/O capture, child lifecycle and cancellation handling.

The rebuild is filesystem-only by design; zvol/block-volume paths are rejected. No generalized dummy filesystem backend
is part of the required architecture.

### Object-store and transfer pipeline

* `store.rs`: object-store trait for HEAD/GET/PUT/list, conditional create and multipart operations.
* `http_store.rs`: S3-compatible HTTP transport with SigV4 signing, configurable metadata header prefix, endpoint,
  addressing mode, and signing service.
* `transfer.rs`: lock handling, bounded multipart buffering, retries for retryable transport failures and commit checks.

The object-store layer is implemented without AWS SDK dependencies. Supported credential inputs are documented in
README; this design does not imply full AWS provider-chain parity.

### Crypto and end-to-end workflows

* `crypto.rs`: per-backup key generation, streaming AEAD (`AES128_GCM_HKDF_1MB`) and GPG key wrap/unwrap helpers.
* `backup.rs`: lock acquisition, base selection, metadata publication, encrypted stream upload and commit resolution.
* `restore.rs`: Prepare → Verification → Replay flow, including stdout export mode.
* `rate.rs`: optional ciphertext throughput limiting in the backup pipeline.
* `cli.rs`/`main.rs`: command-line parsing and top-level orchestration.

Plaintext backup keys, metadata and stream data must remain in memory or pipes, not application-managed temporary
files. This does not claim to control operating-system facilities such as swap or core dumps.

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

### Acceptance evidence mapping (current)

The table below maps the normative scenarios to concrete tests/scripts. It records evidence sources, not provider-wide
compatibility guarantees.

| Scenario row | Evidence sources |
| --- | --- |
| Full and multi-step incremental recovery | `tests/support/zfs_s3_e2e.sh` (full + `s1/s2/s3` replay), plus `tests/zfs_backend.rs` and `src/prepare.rs` chain planning tests |
| Incomplete remote chain | `tests/support/zfs_s3_e2e.sh` (removed remote `s1` path: empty target reject + matching local-base continuation) and `src/prepare.rs::local_declared_base_needs_no_remote_parent` |
| Existing target has changes, or `zfs diff` fails | `src/prepare.rs::dirty_target_stops_before_any_verification_download`, `src/prepare.rs::diff_command_failure_stops_before_verification`, and e2e dirty-target rejection in `tests/support/zfs_s3_e2e.sh` |
| No matching latest local base | `src/prepare.rs::existing_target_without_matching_latest_is_rejected` |
| Wrong parent GUID, metadata mismatch or dependency cycle | `src/prepare.rs::parent_mismatch_and_cycles_are_errors`, metadata/auth checks in `src/restore.rs::verify` and `tests/crypto_stream.rs` |
| Valid prefix followed by corruption or truncation | `tests/crypto_stream.rs::authenticates_aad_segments_and_final_segment`, `tests/crypto_stream.rs::verifies_prefix_with_known_object_length` |
| Send, encryption, part or log upload failure | `src/backup.rs::publication_failure_matrix_and_authenticated_export`, `src/transfer.rs::small_object_bound_and_cancelled_upload` |
| Concurrent writers or existing backup content | `src/transfer.rs::only_one_writer_and_partial_content_refused`, plus opt-in real endpoint check `tests/live_http.rs::real_s3_conditions_ranges_and_multipart` |
| Unknown completion result or stale lock | `src/transfer.rs::completion_response_loss_requires_matching_object`, `src/backup.rs::publication_failure_matrix_and_authenticated_export` |
| Receive fails after earlier chain steps | failure-handling assertions in `src/restore.rs`/`src/prepare.rs` tests and e2e replay checks in `tests/support/zfs_s3_e2e.sh` |
| Stdout export | `tests/support/zfs_s3_e2e.sh` stdout single-stream receive path; backup/export failure matrix in `src/backup.rs::publication_failure_matrix_and_authenticated_export` |
| Small and large streams, retries and service limits | `src/transfer.rs::multipart_limits_and_exact_ciphertext`, `src/transfer.rs::generated_large_stream_has_fixed_buffer_budget`, `tests/zfs_rate.rs`, `tests/http_store.rs` (retry/status/range/multipart capability behavior) |

Additional implementation evidence:

* Tink interop vectors and bidirectional runtime compatibility: `tests/crypto_stream.rs`
* Opt-in real HTTP object-store smoke coverage: `tests/live_http.rs` (requires isolated endpoint/credentials)
* Real ZFS + local S3 end-to-end script: `tests/support/zfs_s3_e2e.sh` (safe namespace only)

Provider scope note: local SeaweedFS/OpenZFS acceptance and offline tests do **not** imply live verification on AWS S3,
MinIO, Backblaze B2, Alibaba OSS, or universal S3 compatibility. Treat each provider as unverified until separately run.

---