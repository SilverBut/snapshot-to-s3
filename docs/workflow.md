# Workflow

Be aware that the plain text encryption key shall never leave on disk. Always use memory or pipe.

## Backup

### Prepare

Parse `zfs:` backup source and readout pool name, filesystem name, and snapshot name.

According to [storage format description](storage.md), ensure the bucket is accessible and acquire the snapshot's
lock atomically. While holding it, ensure no backup content exists under the target prefix, excluding the held lock.
Refuse both existing backups and partial content; do not overwrite them or take over an existing lock.

Use method described in [Find parent snapshot automatically](#find-parent-snapshot-automatically), decide whether this
is an incremental or full send.

Collect necessary info of all related snapshot & volumes for debugging purpose and prepare the JSON content in memory,
without a plaintext temporary file.

Generate a random key. Encrypt the key with GPG key selected.

Generate proper context structs to access the filesystem and object storage. 

### Find parent snapshot automatically

Obviously, if user use `--force-full-snapshot`, we should skip this step and send a full snapshot directly.

Normally, to minimize backup size, the parent snapshot is automatically choose by the following steps:

1. Get a list of local snapshots. Only consider snapshots earlier than the current one and valid as incremental
bases. For each local snapshot, filter out if the remote committed stream is missing or its ID doesn't match.
2. Get candidates by two methods:
  1. Least 4 candidates by read `written` property (like command `zfs get written@old_snapshot new_snapshot`) to see 
  which snapshot have less size differences.
  2. Least 4 candidates by read `createtxg` property (like command `zfs get createtxg old_snapshot`) to see which 
  snapshot is near to current one.
3. Deduplicate the two groups, using fewer candidates when fewer are available. Now we have at most 8 candidates.
For each candidate, use `zfs send -nP -w -i old_snapshot current_snapshot` with the same send flags as the actual
backup to compare estimated sizes. Choose the smallest estimate among these candidates, not a global optimum.

If a candidate disappears or is no longer a valid send base, record the reason and exclude it. If no eligible
candidate remains, send a full backup. Fail explicitly on operational errors such as unreadable properties,
permission errors or failed command execution; do not hide them by silently falling back to a full backup.
This selection does not require checking the base's entire remote dependency chain. Periodic full backups,
dependency retention and backup inspection are external responsibilities.

### Send backup

Upload `key.gpg`, `key.sha256sum` and `meta.json.encrypted` first. The metadata file `meta.json` provides all info about
snapshot current being backup, and the incremental base if any. Both snapshot GUIDs and the base stream's object key
**must** be saved according to [storage.md](storage.md) for verification. Other metainfo can put there too if they are
available before sending backup data.

Create the multipart upload job according to the [storage format description](storage.md) with proper metadata set, 
then stream encryption the backup stream to `stream.encrypted`. The backup stream is retrieved by `zfs send -w`, with
`-i base_snapshot` for an incremental send. Raw send preserves native encryption for an encrypted dataset, independently
of the backup encryption; it does not add native encryption to an unencrypted dataset.

Upload the parts without completing the multipart upload. Once ZFS send has exited successfully, encryption has
finished and all parts are uploaded, encrypt and upload the available log as `backup.log.encrypted`. Then complete
the stream multipart upload as the backup's commit point. Use bounded buffers and retain identical ciphertext for
part retries, as specified in [storage.md](storage.md).

Confirm the commit, report the final result outside the encrypted log, and release the lock. Ensure files under the
target prefix match [storage.md](storage.md). Before commit, any send, encryption, part or log failure prevents
publication. Abort an unfinished upload and report residual objects and cleanup errors. Unknown completion results
and lock-release failures follow the distinct states in the storage specification, not a generic upload failure.

## Restore

ZFS requires a linear snapshot history, which makes our life harder.

### Prepare 

Parse the backup source identifier and readout pool name, filesystem name, and snapshot name. Use the original source
names and S3 prefix to locate the snapshot's `stream.encrypted`, then HEAD it. We call the objects under that prefix
the *source backup*, and our final goal is to restore this backup.

If restore target is `stdout`, preparation ends here. Go to [Verification Backup](#verification-backup) directly for
this single backup; do not build a chain or check a local ZFS target.

For a ZFS target, first ensure the target pool exists and inspect the target filesystem and its snapshots, including
the latest snapshot's GUID if present. Form a *source chain* starting from the source stream's metadata. For each
incremental, follow `base-object-key` with a HEAD request and verify that the parent GUID matches `base-snapshot-id`.
Do not list and HEAD every remote snapshot.

Stop at a full backup, or when the latest local snapshot matches the current or declared base GUID. In the latter
case, omit that local snapshot and all earlier backups from replay; there is no need to access its older remote
dependencies. If a required parent is genuinely missing, warn that the remote chain cannot recover an empty target,
then decide whether a matching local base permits recovery. Permission and network errors are not missing parents:
report them as errors. Reject cycles, malformed base fields and mismatched parent GUIDs.

Then we have two possible scenarios:

1. If target ZFS filesystem does not exist: the *source chain* must end at a full backup to allow recovery.
If a required parent is missing, raise an error: these remote backups cannot recover this empty target.
2. If target ZFS filesystem already exists: its *latest* snapshot GUID must match a snapshot or declared base in
the *source chain*, so that we can cut the chain at this local snapshot. An existing filesystem without snapshots
has no usable base and must be rejected. There are two possible exceptions:
  1. The *source chain* completely have no common snapshot ID with target ZFS filesystem. Suggest user find another
  target ZFS filesystem for a full recovery. Raise error and exit.
  2. There do have a common snapshot ID, but it's not the latest one. Suggest user clone from this common snapshot ID
  then perform recovery on that new filesystem. Raise error and exit.

After confirming the latest local snapshot as the receive base, run
`zfs diff -H target_filesystem@latest_snapshot target_filesystem` within this Prepare stage. Successful execution with
no change records is required. If it reports any changes, stop with an error explaining that the target has changed
since its latest snapshot. If the command itself fails, report the check failure and stop.

Both failures occur **before Verification Backup**: do not download `key.gpg`, `key.sha256sum`, encrypted metadata or
stream data for verification. This filesystem path-difference check is not a general zvol check. A new target and
stdout export do not require it.

If the target is already at the requested snapshot, apply this same existing-target check, then report that no replay
is needed. Otherwise proceed to verify only the backups selected for replay.

The user must prevent concurrent writes throughout restore. A clean diff is an early precondition check, not a promise
that the state cannot subsequently change or that all raw receive constraints are satisfied. ZFS receive remains the
final authority; do not automatically use `-F` to roll back or discard target changes.

### Verification Backup

Now either we got a *source chain* of backups to replay, or a single *source backup* for stdout. They are all object
groups following [storage specification](storage.md).

For each required backup, download `key.gpg` and `key.sha256sum`. Decrypt key and verify if checksum matches. If key can
not be decrypted or the checksum does not meet, report error.

Use `key` to decrypt `meta.json.encrypted`. Ensure its GUIDs, base object key and other indexing fields agree with S3
metadata and the *source chain*, using the full/incremental field rules in [storage.md](storage.md). If not, report an
error. Hash the exact decrypted JSON bytes, not a reserialized representation.

Using `key` and checksum of `meta.json` as AAD, we now can decrypt the `stream.encrypted`. Only decrypt first 1~2MiB
as a test, reading and authenticating complete initial segments rather than releasing unauthenticated plaintext.

If any backup fails these checks, report an error. This preflight checks the key, metadata and stream prefix, not
the entire stream. No separate full-stream verification pass is required before replay.

### Replay stream

After all files prepared and verified, we can now replay stream.

Restore to `stdout` performs the same decryption described in [verification](#verification-backup), writing the selected
backup's send stream only. An incremental stream still requires its base when later passed to ZFS; no dependencies are
concatenated here. Write diagnostics to stderr. An export may emit authenticated segments before a later failure;
report incomplete export with a nonzero exit status.

To restore to the zfs, we need to traverse the source chain in reverse, one by one. Call `zfs receive -u` for each
decrypted stream, leaving mounting to the user, and report error if any.

Every replay must finish authenticated decryption, including the final segment, and have a successful receive exit
status. Authentication errors, truncation and receive errors must not be treated as normal EOF or success, even if
prefix verification passed. Stop on failure and identify the failed backup and last successfully received snapshot.
Earlier completed steps are not automatically rolled back.

Successful ZFS restore means that the required streams have been fully decrypted and received. It does not require
loading native encryption keys, mounting the filesystem or checking application data.