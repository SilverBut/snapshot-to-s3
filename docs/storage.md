# Storage format

## Layout

A backup of `dataset@snapshot` to `s3://bucket/prefix` is a group of objects under
`prefix/dataset/snapshot/`:

| Object | Content |
| --- | --- |
| `key.gpg` | The 32-byte backup key, encrypted to the GPG recipient |
| `key.sha256sum` | Lowercase hex SHA-256 of the key and one LF (a consistency check, not authentication) |
| `meta.json.encrypted` | Encrypted [backup metadata](#metadata) |
| `backup.log.encrypted` | Encrypted diagnostics (at most 256 KiB): sizes, object count, base candidates |
| `stream.encrypted` | The encrypted `zfs send -w` stream, or its first part; carries the index metadata |
| `stream.encrypted.000001`, … | Continuations of a stream larger than one object |
| `.lock` | Writer lock; exists only while a backup runs or after an unresolved failure |

Objects are never tar archives and are never overwritten.

## Encryption

Every `*.encrypted` object is a separate Tink-compatible `AES128_GCM_HKDF_1MB` stream (HKDF-SHA256,
16-byte AES key, 1 MiB ciphertext segments, fresh salt and nonce prefix per object) under the backup key.
The associated data of the stream is the raw SHA-256 digest of the exact plaintext `meta.json` bytes;
other objects have empty associated data. Segment nonces bind each segment's index and whether it is the
last, so truncation, reordering, and missing or extra data fail authentication. The 32-bit segment
counter limits one stream to about 4 PiB.

## Stream objects

The ciphertext is one byte sequence split over `stream.encrypted` and, if it does not fit in one object,
continuation objects `stream.encrypted.NNNNNN` (from `000001`, six digits, at most 999,999). Each object
is filled up to `--max-parts` parts and `--max-object-size` bytes before the next starts. Continuations
have no metadata. A reader concatenates `stream.encrypted` and the continuations up to the first absent
number. Backups that fit in one object have no continuations, so the format is unchanged for them.

## Metadata

`stream.encrypted` has this user metadata (shown with the default `x-amz-meta` prefix). It lets backup
and restore follow chains with `HEAD` requests alone:

| Header | Value |
| --- | --- |
| `x-amz-meta-gpg-key-id` | Full fingerprint of the recipient |
| `x-amz-meta-fs-type` | `zfs` |
| `x-amz-meta-vol-id` | GUID of the source filesystem |
| `x-amz-meta-current-snapshot-id` | GUID of the snapshot |
| `x-amz-meta-base-snapshot-id` | GUID of the incremental base (incremental only) |
| `x-amz-meta-base-object-key` | Bucket-relative key of the base's `stream.encrypted` (incremental only) |

GUIDs are exact decimal strings. The decrypted `meta.json` repeats these fields without the prefix
(using JSON `null` for the base fields of a full backup) and adds `source-dataset` and `source-snapshot`:

```json
{
  "gpg-key-id": "4E8BBEB1DF8D5C3CCA2F6B51ED7C55E60B7543A2",
  "fs-type": "zfs",
  "vol-id": "14066916649205506663",
  "current-snapshot-id": "12631906673493169747",
  "base-snapshot-id": "16158825896409765662",
  "base-object-key": "backups/pool/data/s1/stream.encrypted",
  "source-dataset": "pool/data",
  "source-snapshot": "s2"
}
```

The object metadata is an unauthenticated index for planning. Restore uses it only after checking that
it agrees with the authenticated `meta.json`.

## Lock and commit protocol

1. Normally create `.lock` with a conditional `PUT`, containing a random token. By default this uses
   `If-None-Match: *`; `--lock-detection-mode x-cos-forbid-overwrite` uses COS's
   `x-cos-forbid-overwrite: true` instead. If the lock exists, stop. A `HEAD` followed by a `PUT` is
   not a lock, so services without conditional create are rejected.
2. Holding the lock, require that the prefix contains nothing else.
3. Upload `key.gpg`, `key.sha256sum` and `meta.json.encrypted`.
4. Start the multipart upload of `stream.encrypted` and upload its parts. Upload, complete and confirm
   (by `HEAD` size) each continuation object. Do not complete `stream.encrypted` yet.
5. After `zfs send`, encryption and all uploads succeed, upload `backup.log.encrypted`.
6. Complete `stream.encrypted` and confirm it with `HEAD` (metadata and size). **This is the commit
   point**: a prefix without `stream.encrypted` is never a valid backup or base.
7. Delete the lock.

The lock never expires and is never taken over. An operator may delete a stale lock or partial backup only
after making sure that its writer and uploads have stopped.

`--lock-detection-mode dangerously-skip` is an unsafe alternative: it does not create, acquire, or
release `.lock` and permits concurrent writers to race. The prefix is still listed and must be empty
before upload, but that check is not atomic.

Unless locking is skipped, the selected mode is checked before every backup using a temporary probe
object. COS mode requires `x-cos-forbid-overwrite` to reject an overwrite with the `FileAlreadyExists`
error. Tencent COS documents that this header does not prevent overwrites when bucket versioning is
enabled; the capability probe rejects such a configuration if the header is ignored. Skip mode omits
the conditional-create probe but still checks metadata preservation.

Failure handling depends on what is known about the `stream.encrypted` upload:

| State | Action |
| --- | --- |
| Not started, or definitely rejected | Release the lock |
| Open (not completed) | Abort it; release the lock if the abort succeeded |
| Initiation outcome unknown | Keep the lock |
| Completion sent, outcome unknown | Check with `HEAD`: a matching object means committed; otherwise keep the lock and report the state as unknown |

An unfinished continuation upload is aborted. Residual objects are listed on stderr. A later backup
of the same snapshot refuses the prefix until the partial content is removed. If the lock cannot be deleted
after a confirmed commit, the error says `backup committed, lock cleanup failed`.

With `dangerously-skip`, unresolved upload outcomes have no lock protecting the prefix; stop other
writers and inspect the upload and objects manually before retrying.

## Multipart uploads

Parts hold ciphertext directly. The part size is sized from the `zfs send -nP` estimate to fill one object in
`--max-parts` parts, at least 8 MiB and `--min-part-size` (default 100 MiB), and at most `--part-buffer-size` and
`--max-part-size`. Parts grow after half of an object's parts are used, so an underestimated stream needs
fewer objects. A part that fails with a transient error is retried up to twice with the same bytes. Only the final ETag of each part is
kept. ETags are not used as content checksums.

## HTTP transfers

PUT and POST requests always include the exact `Content-Length`, including `0` for empty
capability-probe PUTs and multipart-initiation POSTs, for compatibility with endpoints that require it.

GET, POST and part PUT requests have no total timeout. Instead, a rolling throughput guard fails a
transfer that moves fewer than 1,024 bytes in 30 seconds. Time that a GET waits on its consumer is not
counted. HEAD, LIST and DELETE have a 120-second timeout. Connections time out after 10 seconds.

A GET pins the object's strong ETag. After a transient failure (connection error, 408, 429, 5xx, early
EOF or throughput failure), it resumes at the exact offset with a signed `Range` and `If-Match`. A changed
object, range or length is an error. The retry budget (default 3) applies to each object read and is
not reset by progress. Other requests are not retried automatically. A failed `PUT` or `POST` does not
mean the request was rejected; completion follows the unknown-outcome rule above.

| Variable | Default | Meaning |
| --- | ---: | --- |
| `SNAPSHOT_TO_S3_HTTP_WINDOW_SECS` | 30 | Throughput window, seconds (1–86,400) |
| `SNAPSHOT_TO_S3_HTTP_MIN_BYTES` | 1024 | Minimum bytes per window |
| `SNAPSHOT_TO_S3_HTTP_CONTROL_TIMEOUT_SECS` | 120 | HEAD/LIST/DELETE timeout, seconds (1–86,400) |
| `SNAPSHOT_TO_S3_HTTP_GET_RETRIES` | 3 | GET retries per object (0–100) |
| `SNAPSHOT_TO_S3_HTTP_BACKOFF_MILLIS` | 200 | First retry delay, doubling up to 60 s (0–60,000) |

Library users can pass an `HttpPolicy` to `HttpStore::new_with_policy`.
