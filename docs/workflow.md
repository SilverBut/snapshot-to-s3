# Workflow

Object names, the lock and commit protocol are defined in [storage.md](storage.md).

## Backup

1. **Check.** Validate limits and the source (an existing filesystem snapshot).
2. **Lock.** Acquire `.lock` and require an otherwise empty prefix. With `--force-overwrite`, delete every other
   object under the prefix instead; the lock itself is still acquired exclusively. Acquisition verifies
   conditional create. Then check user metadata using the sibling probe objects described in
   [storage.md](storage.md#lock-and-commit-protocol); optionally also probe multipart metadata.
3. **Select a base** (skipped with `--force-full-snapshot`), as described below.
4. **Write the key and metadata.** Resolve the GPG selector to one fingerprint. Generate a key, and
   upload `key.gpg`, `key.sha256sum` and `meta.json.encrypted`.
5. **Stream.** Start the multipart upload of `stream.encrypted`, carrying the index metadata. Run
   `zfs send -w [-i base]` through encryption and the rate limiter into parts. When an object is full,
   continue in completed continuation objects.
6. **Commit.** Once the send, encryption and all uploads have succeeded, upload `backup.log.encrypted`.
   Then complete `stream.encrypted`, confirm it with `HEAD`, and delete the lock. The log always includes
   expected stream metadata. A published, correctly sized stream with missing or wrong metadata is kept,
   with a nonzero exit and repair diagnostics, rather than treated as an unknown commit.

Raw send keeps native ZFS encryption when the source has it; it does not add native encryption.

### Base selection

1. List older local snapshots of the dataset. Keep those whose committed `stream.encrypted` exists at the
   same prefix with matching snapshot and filesystem GUIDs.
2. Shortlist up to 4 candidates with the smallest `written@candidate` and up to 4 with the closest
   `createtxg`.
3. Estimate each shortlisted candidate with `zfs send -nP -w -i candidate snapshot` and use the smallest.
   This is the best of the shortlist, not necessarily of all snapshots.

Candidates that disappear or are not valid bases are skipped and recorded in the log. If none remains,
the backup is full. Candidates with missing or invalid stream metadata are also skipped with a warning
and log diagnostic; repair their metadata before using them as bases. Operational errors (permissions,
failed commands, unreadable properties, S3 errors)
abort the backup instead of falling back to a full send.

## Restore

### Prepare

1. `HEAD` the selected `stream.encrypted` at the backup prefix, using the original source names.
2. For stdout export, the plan is this one backup.
3. For a ZFS target, inspect the target and its latest snapshot. Follow `base-object-key` with `HEAD`
   requests, checking each parent GUID, until a full backup or a backup that the target's latest
   snapshot already provides. Cycles and malformed or mismatched base fields are errors. Permission and
   network errors are errors, not missing parents.
4. Decide:
   * New target: the chain must reach a full backup.
   * Existing target: its latest snapshot must be the chain's starting point. Otherwise restore fails,
     suggesting another target or a clone of a common snapshot. A missing remote parent is only a
     warning when the local base makes it unnecessary.
5. For an existing target, run `zfs diff -H target@latest target`; any change or failure stops the
   restore before any key or stream download. If the target already has the requested snapshot,
   report that nothing needs replaying.

### Verify

For every planned backup, before replaying any:

1. Download and unwrap the key with GPG, and check `key.sha256sum`.
2. Decrypt `meta.json.encrypted` and check that it matches the object metadata and the chain. Decrypt
   and authenticate the log.
3. Find the stream's continuation objects by `HEAD` and authenticate the first 2 MiB of the stream.

This checks keys, metadata and the stream start. It does not verify whole streams in advance.

### Replay

Replay the chain oldest first. Read each stream's objects in order, pinned to the ETags seen during
verification, decrypt it and pipe it into `zfs receive -u`. Then check that the received snapshot has
the authenticated GUID. Stdout export writes the one decrypted stream instead; an incremental stream
still needs its base when received later.

Every stream must authenticate through its final segment and `zfs receive` must succeed. On failure,
restore stops and reports the failed backup and the last received snapshot. Received snapshots are kept.
The user must prevent writes to the target during restore. The `zfs diff` check does not guarantee that
`zfs receive` will succeed, and `-F` is never used.
