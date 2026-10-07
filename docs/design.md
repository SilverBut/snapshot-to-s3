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

* Genearte a random key
* For a given key, create a encrytion interface which accepts a stream input and send encrypted stream out
* GPG related
    * Find (public) encyrpt-capable key indicated by user id or key id
    * Encrypt with the key

All functions in `crypto/` should not use any on-disk temporary file by default, unless method signature strongly 
indicates so.

`utils/` is a set of tools. `utils/mbuffer.rs` provides [mbuffer][mbuffer] ability so we can apply speed limit.

## Stream Tar for Multipart Upload

We all know that for a S3 compatible object storage, *multipart upload* is a common feature. It's typical use case is:

* Get a upload ID first
* Cut a file into series of small parts with increasing part numbers (>5MiB each part except for last piece)
* Upload each part with upload ID and part number, returning a ETag
* Finialize the multipart upload with upload ID, all part numbers and their ETags
* Object stoarge will concatenating the parts in ascending order based on the part number

It can help us to upload a large `tar` file, but our problem is our file (or stream) size, can't be known in advance:

```
[tar header with file length]
[file content at length]
[optional padding]
[tar header with file length]
[file content at length]
[optional padding]
...
[eof marker]
```

Fortunately, most S3 object storage allows overwrite any previous upload part, so actually we can:

1. Upload a pseudo tar header where everything set but size filled to 0
2. Upload stream content with padding. We can know the stream size meanwhile.
3. Overwrite the part written in step 1 and fill size back.

Normally, the uploaded part can not be retrieved again, so the part with header will be uploaded later than content, 
which seems weird but totally legit. This also means the part size is dynamic:

- Size of last part is fine as long as it does not beyond maxium
- Part with header should be as small as possible but >= 5MiB to meet the minimal part size requirement
- Other data part should be larger (500MiB for example)

---

[mbuffer]: https://www.maier-komor.de/mbuffer.html