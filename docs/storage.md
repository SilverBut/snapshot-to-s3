# Storage layout

Each backup creates objects at prefix `s3://${prefix}/{dataset_name}/{snapshot_name}/`.

Files under this prefix are:

- `key.gpg` - Backup encryption key (GPG-encrypted)
- `key.sha256sum` - SHA256 sum for decrypted content of `key.gpg`
- `meta.json.encrypted` - Backup metadata
- `stream.encrypted` - Snapshot data
- `backup.log.encrypted` - Backup log

`key.gpg` contains the per-backup encryption key (we will call it `key`) and encrypted by given GPG key. 
Its decrypted content is exactly 32 random bytes, without a text encoding. `key.sha256sum` is the lowercase
64-character hexadecimal SHA256 digest of those bytes, followed by one LF, without a filename. This checksum is an
auxiliary consistency check, not a replacement for authenticated decryption.

All other `*.encrypted` files are encrypted by `key` using [AES-GCM-HKDF][aes-gcm-hkdf], or `AES128_GCM_HKDF_1MB` in
a more precision way to describe it. This allows ~2000TiB encryption per file, which is usually enough. 

This is a cryptographic capacity statement, not the object-size limit of the selected storage service.

Each encrypted object uses an independent AES-GCM-HKDF streaming header with fresh salt and nonce prefix, using
SHA256, a 16-byte derived AES key and 1,048,576-byte ciphertext segments. The backup key is shared only within this
backup, not the encryption state of an individual object.

Associated data of the stream is the raw 32-byte SHA256 digest of the exact plaintext bytes of `meta.json` before
encryption, not its hexadecimal text or a reserialized JSON document. Other encrypted files have empty associated
data.

The following metadata is stored for `stream.encrypted` (using the configured metadata prefix, default `x-amz-meta`):

- `x-amz-meta-gpg-key-id` - GPG key identifier used for encryption
- `x-amz-meta-fs-type` - File system type. Now fixed at `zfs`
- `x-amz-meta-vol-id` - Volume GUID of the snapshot's parent filesystem
- `x-amz-meta-current-snapshot-id` - Current snapshot GUID
- `x-amz-meta-base-snapshot-id` - Base snapshot GUID, if this is an incremental backup
- `x-amz-meta-base-object-key` - Complete object key of the base backup's `stream.encrypted` in the same bucket

GUID values are decimal strings. Full backups omit both base fields; incremental backups require both. The parent
key is bucket-relative, including the configured prefix, not an S3 URI or a path relative to the current snapshot.
These metadata enable automatic incremental backup detection and direct chain traversal using HEAD requests.

The encrypted `meta.json` contains these same logical fields without the HTTP metadata prefix:
`gpg-key-id`, `fs-type`, `vol-id`, `current-snapshot-id`, `base-snapshot-id` and `base-object-key`. It also records
`source-dataset` and `source-snapshot`, using the original backup names. Full backups use JSON null for both base
fields; incrementals use strings. Additional diagnostic properties may be included.

During verification, these fields must agree with the stream object's metadata and the selected source chain.
Object metadata is an index for preparation, not a substitute for this check. Source volume identity is not the
identity of the destination dataset in another pool.

## Lock and publication

The lock object is `${snapshot_prefix}.lock`, for example `backups/dataset-snapshot/vp1/guid_lab/alpha/s1/.lock`.
It is not one of the five backup content objects.

1. Create the lock atomically only if it does not exist, for example using `PutObject` with `If-None-Match: *`.
   Store a random ownership token in its content. If the service cannot provide this primitive, report an error;
   HEAD followed by an unconditional PUT, or delayed read-back checks, is not a lock.
2. After acquiring the lock, check that the snapshot prefix contains no objects other than this lock. Existing
   committed or partial backup content causes an error; it must not be overwritten or reused.
3. Upload `key.gpg`, `key.sha256sum` and `meta.json.encrypted`.
4. Upload the encrypted stream's multipart parts, but do not complete the upload yet.
5. Confirm that ZFS send, encryption and all part uploads succeeded, then upload `backup.log.encrypted`.
6. Complete the stream's multipart upload. Do not use a conditional completion request; the held lock serializes
   participating writers. Confirm the result before declaring the backup committed.
7. Report the final result outside the encrypted log and release the lock.

Publication of `stream.encrypted` is the commit point. Partial prefixes without this object are not valid backups or
incremental bases. The other four objects must already have been uploaded before commit. Readers must still verify
their required objects and authenticated content; commit is not a guarantee against subsequent loss or corruption.
The encrypted log covers stages before commit, not its own upload result or the final completion result.

The lock has no automatic expiration or takeover. Read-back of its token can identify ownership but cannot replace
atomic acquisition. The non-overwrite guarantee applies to writers following this protocol, not independent tools
that overwrite or delete objects. An operator may remove a stale lock or partial content only after confirming
that its writer and upload are no longer active.

On a definite pre-commit failure, stop producers, abort the multipart upload if initiated, report residual objects
and any cleanup errors, and release the lock only when the upload is known to be stopped. Auxiliary objects may
remain; a later attempt must refuse this partial prefix until it is cleaned.

If completion times out or its result is otherwise unknown, inspect the final stream object under the held lock and
compare its metadata and length with the expected upload. A matching object confirms commit. An absent object
alone does not prove that an in-flight completion has stopped. If the outcome cannot be resolved, report it as
unknown, retain the lock and do not blindly retry or remove objects.

Once commit is confirmed, failure to delete the lock is reported as "backup committed, lock cleanup failed", with a
nonzero exit status indicating required operator attention, rather than claiming the backup was not uploaded.

## Multipart stream

Stream parts contain ciphertext directly; no tar headers or length backfilling are needed. Part sizes may be selected
and adjusted using the send estimate and the service's limits. Account for part numbers already consumed; growing
later parts cannot reclaim them. All non-final parts must satisfy the service's minimum size.

Buffers, concurrent uploads and retained retry parts must have explicit bounds. Retry the exact retained ciphertext
bytes. Keep the final ETag for each part in the completion list; an ETag is not an authenticated content checksum.
Report an error before exceeding part-count, part-size or object-size limits, and do not commit an incomplete stream.

## HTTP transfer liveness and recovery

Object GET, POST and transfer PUT requests have no whole-transfer duration limit. The HTTP client has a
10-second connection timeout. A rolling throughput guard fails an actively polled transfer when fewer than
1,024 bytes progress in the preceding 30 seconds, including continuously dribbling connections, not only idle
connections. The first window is a startup allowance; earlier bursts do not buy unlimited time. GET consumer
backpressure (for example, decryption or ZFS receive) is excluded from the measurement. Upload progress is measured
as bounded 64-KiB body chunks are accepted by the HTTP transport, not as proof of durable service receipt.
Waiting for response headers or a response body is also guarded. HEAD, list and DELETE control requests additionally
have a 120-second request timeout.

GET transparently retries transient connection, HTTP 408/429/5xx, premature EOF, response-read and throughput
failures. The default budget is three retries for the entire reader, including initial request attempts, with
200-ms exponential backoff (exponent capped at eight, each delay capped at 60 seconds). Retry requests resume at the exact consumed ciphertext
offset using a signed Range and If-Match. Buffered ciphertext is drained before resuming. The first response must
provide Content-Length and a strong ETag, which is pinned even when the caller did not provide If-Match. Every
response is validated before exposing bytes: status, ETag, identity content encoding, exact Content-Range,
Content-Length and total object size must agree. Caller byte-range endpoints are retained; invalid or changed
identity is a terminal error, not a reason to restart from zero.

Mutations are not automatically retried by the HTTP store. A PUT/POST network or throughput failure does not
prove rejection: completion may have committed and must still follow the unknown-outcome protocol above.
The multipart uploader may retry only its retained, identical part ciphertext according to its existing policy.

The CLI accepts these optional unsigned-integer environment overrides:

| Variable | Default | Meaning |
| --- | ---: | --- |
| `SNAPSHOT_TO_S3_HTTP_WINDOW_SECS` | 30 | Rolling measurement window, seconds (1–86,400) |
| `SNAPSHOT_TO_S3_HTTP_MIN_BYTES` | 1024 | Minimum progress per window (positive) |
| `SNAPSHOT_TO_S3_HTTP_CONTROL_TIMEOUT_SECS` | 120 | Control-request timeout, seconds (1–86,400) |
| `SNAPSHOT_TO_S3_HTTP_GET_RETRIES` | 3 | Total GET retries (0–100) |
| `SNAPSHOT_TO_S3_HTTP_BACKOFF_MILLIS` | 200 | Initial retry delay, milliseconds (0–60,000) |

Library users can instead pass `HttpPolicy` to `HttpStore::new_with_policy`, including subsecond windows for
focused tests. Invalid policy values fail before any object-store request.

## Examples

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

The resulting backup objects would be under:

```bash
s3://my-backup-bucket/backups/dataset-snapshot/vp1/guid_lab/alpha/s1/
```

And it would have these metadata for `stream.encrypted`:

- `x-amz-meta-gpg-key-id`: `4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2`. Note here ID is expanded to full format.
- `x-amz-meta-fs-type`: `zfs`
- `x-amz-meta-vol-id`: `14066916649205506663`
- `x-amz-meta-current-snapshot-id`: `16158825896409765662`

Both base fields are absent because this is a full backup.

If snapshot `s2` is also uploaded and if `s1` is selected as base, the new backup objects would be under:

```bash
s3://my-backup-bucket/backups/dataset-snapshot/vp1/guid_lab/alpha/s2/
```

With these metadata for `stream.encrypted`:

- `x-amz-meta-gpg-key-id`: `4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2`. Note here ID is expanded to full format.
- `x-amz-meta-fs-type`: `zfs`
- `x-amz-meta-vol-id`: `14066916649205506663`
- `x-amz-meta-current-snapshot-id`: `12631906673493169747`
- `x-amz-meta-base-snapshot-id`: `16158825896409765662`
- `x-amz-meta-base-object-key`: `backups/dataset-snapshot/vp1/guid_lab/alpha/s1/stream.encrypted`

The decrypted metadata for this incremental backup includes:

```json
{
  "gpg-key-id": "4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2",
  "fs-type": "zfs",
  "vol-id": "14066916649205506663",
  "source-dataset": "vp1/guid_lab/alpha",
  "source-snapshot": "s2",
  "current-snapshot-id": "12631906673493169747",
  "base-snapshot-id": "16158825896409765662",
  "base-object-key": "backups/dataset-snapshot/vp1/guid_lab/alpha/s1/stream.encrypted"
}
```

---

[aes-gcm-hkdf]: https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming?hl=zh-cn