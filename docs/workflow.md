# Workflow

Be aware that the plain text encryption key shall never leave on disk. Always use memory or pipe.

## Backup

### Prepare

Parse `zfs:` backup source and readout pool name, filesystem name, and snapshot name.

According to [storage format description](storage.md) ensure bucket is accessible and no existing snapshot backup
file exists.

Use method described in [Find parent snapshot automatically](#find-parent-snapshot-automatically), decide whether this
is an incremental or full send.

Collect necessary info of all related snapshot & volumes for debugging purpose and prepare them as a JSON file.

Generate a random key. Encrypt the key with GPG key selected.

Generate proper context structs to access the filesystem and object storage. 

### Find parent snapshot automatically

Obviously, if user use `--force-full-snapshot`, we should skip this step and send a full snapshot directly.

Normally, to minimize backup size, the parent snapshot is automatically choose by the following steps:

1. Get a list of local snapshots. For each local snapshot, filter out if remote snapshot missing or ID doesn't match.
2. Get candidates by two methods:
  1. Least 4 candidates by read `written` property (like command `zfs get written@old_snapshot new_snapshot`) to see 
  which snapshot have less size differences.
  2. Least 4 candidates by read `createtxg` property (like command `zfs get createtxg old_snapshot`) to see which 
  snapshot is near to current one.
3. Now we have at most 8 candidates. For each candidate, use command `zfs send -nP -i` to compare which old snapshot can
have smallest size to send. Find the smallest one.

### Send backup

In [design document](design.md) there is a way to upload tar file dynamically using multipart upload. That would be how
we create the backup file. 

Create the multipart upload job according to the [storage format description](storage.md) with proper metadata set, 
then fill backup file according to the same spec.

The metadata file `meta.json` provides all info about snapshot current being backup, and the incremental base if any. 
Both snapshot's ID **must** be saved in `meta.json` for verification. Other metainfo can put there too if they are
available before sending backup data.

The backup stream is retrieved by `zfs send -w`, which sends a raw stream, or a incremental raw stream. This allows us
having additional layer of security in additional to the backup encryption.

After backup stream is send, all currently available log should be appended to the log file and upload, as a part of
backup file. Finalize the tar file and multipart upload. Print the uploaded file info into the log.

## Restore

ZFS requires a linear snapshot history, which makes our life harder.

### Prepare 

Parse `zfs:` backup source and readout pool name, filesystem name, and snapshot name. Use these info and the S3 prefix
to locate archive file in S3. We call this *source archive* and our final goal is to restore this archive.

If restore target is `stdout`, the prepare stage can ends here. Go to [next step](#verification-backup) directly.

Then we first need to form a source chain from S3. List all snapshot files for the source filesystem in S3 and return
their name along with user-defined metadata. Using the snapshot id and base snapshot id, it's easy to trace through a
chain of files started from our *source archive*, until either a full backup is found, or a incremental backup is found
having no base snapshot indexable. The latter one should raise a warning because it mean a disaster recovery might be
impossible, but it won't affect the restore procedure. We call this *source chain*.

Now we focus on local ZFS. Ensure target ZFS pool exists. Then we inspect the target ZFS filesystem (or dataset) along
with its snapshot list. Obviously we can get local snapshot's ID too. Then we only have two possible scenarios:

1. If target ZFS filesystem does not exists: the *source chain* must end at a full backup so we allow a full recovery.
If *source chain* ends at a incremental backup, raise error and this is a unrecoverable backup, unfortunately.
2. If target ZFS filesystem already exists: the *latest* snapshot ID of local ZFS filesystem must exists in the
*source chain*, so that we can cut *source chain* at this snapshot ID. There are two possible exceptions:
  1. The *source chain* completely have no common snapshot ID with target ZFS filesystem. Suggest user find another
  target ZFS filesystem for a full recovery. Raise error and exit.
  2. There do have a common snapshot ID, but it's not the latest one. Suggest user clone from this common snapshot ID
  then perform recovery on that new filesystem. Raise error and exit.

Now we can ensure if we replay *source chain* one by one into the target ZFS filesystem, we can finally reproduce the
*source archive*.

### Verification Backup

Now either we got a *source chain* with multiple elements, or we got a single *source archive*. Any way, they are all
backup archives following [storage specification](storage.md).

For each file, we try stream read the tar file to download `key.gpg` and `key.sha256sum`. Decrypt key and verify if
checksum matches. If key can not be decrypted or the checksum does not meet, report error.

Use `key` to decrypt `meta.json.encrypted`. Ensure ID's saved in `meta.json` are same with values read from S3 metadata,
and they are same with *source chain*. If not, report error. This usually means some weird problem happened.

Using `key` and checksum of `meta.json` as AAD, we now can decrypt the `stream.encrypted`. Only decrypt first 1~2MiB
as a test to ensure the stream works okay.

If any file failed the check above, report error.

### Replay stream

After all files prepared and verified, we can now replay stream.

Restore to `stdout` can be easy, simply perform the same decrypt procedure using ways in 
[verification](#verification-backup) then write all content of plaintext of `stream.encrypted`. Since we only have one
file, this is easy.

To restore to the zfs, we need to traverse the source chain in reverse, one by one. Call `zfs receive` for each
decrypted stream, and report error if any.

Since the stream have passed preflight process during verification, it's highly impossible to have error on it. Besides
we can safely rely on ZFS's receive to report error.