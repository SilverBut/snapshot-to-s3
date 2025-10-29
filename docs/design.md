# Design of program

## Component layout

Note, directories mentioned in this paragraph are relative to the source root.

`fs/` provides compatible layer for different file system. Each struct should implement a set of traits which can:

* List volumes (for `zfs` this means all vol and subvols)
* List snapshots for a volume
* For a volume or snapshot, get its properties and its ID
* Get raw stream of a snapshot, or a diff of two snapshots

Trait inclues `SnapshotableFilesystem`, `Volume`, `Snapshot`. For now, we need two struct `dummy` and `zfs`.

`storage/` mainly wraps client to access S3-compatible object storage to provide those capabilities:

* List files in bucket witha optional prefix
* Stat a file info
* Get a file's user defined metadata (like `x-amz-meta-*` or `x-cos-meta-`, depends on user config)
* Upload a file

In additional to the wrapped client, the module also provides ability to stream upload a tar file. See the section
"Stream Tar for Multipart Upload" below.

`crypto/` is where out encryption reload code resides. It can:

* Genearte a random AES-256-GCM key
* For a given key, create a AES-256-GCM encrytion interface which accepts a stream input and send encrypted stream out
* GPG related
    * Find (public) encyrpt-capable key indicated by user id or key id
    * Encrypt with the key

All functions in `crypto/` should not use any on-disk temporary file by default, unless method signature strongly 
indicates so.

`utils/` is a set of tools. `utils/mbuffer.rs` provides [mbuffer][mbuffer] ability so we can apply speed limit.

## Storage layout

Each backup usually generates one single file:

* `$vol_id/$snapshot_id/backup.tar`, the backup file contains multiple file optionally encrypted by `key`. It contains:
    * `key.gpg`: Encryption key which itself is encrypted by GPG.
    * `meta.json.encrypted`: metadata of this backup
    * `stream.encrypted`: backup stream
    * `log.encrypted`: log for this backup

Some info will be saved user-defined metadata. Assume the provider requires `$metadata_header-` as the prefix
of user defined metadata, here is the list:

* `$metadata_header-gpg-id`: The identifier of GPG key used to encrypt `key`. Can be a key hex ID or user identifier.
* `$metadata_header-mode`: Enum of either `incremental` or `full`.
* `$metadata_header-parent-vol-id`: If this is a incremental, give id of parent volume.
* `$metadata_header-parent-snap-id`: If this is a incremental, give id of parent snapshot.

## Workflow

### Preperation

Generate a random key. Encrypt the key with GPG key selected.

Generate a JSON metadata file, containing necessary info of the backup.

Generate proper context structs to access the filesystem and object storage. 

### Full / Incremental send decision

When receives a snapshot, the program first find out volume ID `$vol_id` and snapshot ID `$snapshot_id`, and consult S3
if an existing backup already exists. We don't allow multiple version exists for the same backup file.

If no file exists, list recent backup for `$vol_id` so can get a list of remote snapshots. Compare with local snapshot
list, and find latest remote snapshot with **all** these conditions met:

* The remote snapshot exists locally
* If remote snapshot is a incremental one, the parent snapshot exists remotely, and also met this condition

This actually forms a chain of snapshots. Walking of this chain can be easily done by read the user-defined metadata. 
If these condition are all met, we can ensure a full snapshot chain can be recovered remotely by using this remote
snapshot as parent. Set it as parent of a incremental send.

If no parent could be found, we need to do a full send.

Call filesystem to prepare a full or incremental send stream.

### Send backup

Create a tar file by multipart upload, and streaming contents one by one. After all files have been send, finalize the
tar file. The multipart upload will ensure the file is only generated if upload succeeds.

### Finialize 

After file is sent, set the user-defined metadata. Clean local storage if any.

## Stream Tar for Multipart Upload

Layout of TAR file (with optional PAX extension) can be generally described as:

```
[tar header with file length]
[file content at length]
[eof marker, optional]
[tar header with file length]
[file content at length]
[eof marker, optional]
...
```

We all know that for a S3 compatible object storage, *multipart upload* is a common feature. It's typical use case is:

* Get a upload ID first
* Cut a file into series of small parts with increasing part numbers
* Upload each part with upload ID and part number, returning a ETag
* Finialize the multipart upload with upload ID, all part numbers and their ETags
* Object stoarge will concatenating the parts in ascending order based on the part number

Most S3 object storage allows you to overwrite any previous upload part. Considering the Tar format, we can create
a tar file from a read-only stream, even if we don't know the stream size, by upload the header later than data.

Here is the pseudocode to do this:

```
const UPLOAD_PART_SIZE = 100_000_000;  // 100MB per part
const EOF_MARKER = generateEofMarker();

uploadID := s3.createMultipartUpload()
partID := 1
partETags := []
for stream := range inputStreams {
    // Record header supposed position.
    headerUploadID := partID
    partID += 1
    partETags = partETags.append("")

    // Upload the file. Meanwhile, calculate size.
    fileSize := 0
    for part := range stream.read_as_iter(UPLOAD_PART_SIZE) {
        fileSize += len(part)
        ETag := s3.putMultipartUploadPart(uploadID, partID, part)
        partID += 1
        partETags = partETags.append(ETag)
    }
    // Append EOF Marker.
    eofETag := s3.putMultipartUploadPart(uploadID, partID, EOF_MARKER)
    partID += 1
    partETags = partETags.append(eofETag)
    
    // Generate and upload header
    tarHeader := generateTarHeader(stream.name(), fileSize)
    partETags[headerUploadID-1] = s3.putMultipartUploadPart(uploadID, headerUploadID, tarHeader)
}
s3.finializeMultipartUpload(uploadID, range(1, partID), partETags)
```


[mbuffer]: https://www.maier-komor.de/mbuffer.html