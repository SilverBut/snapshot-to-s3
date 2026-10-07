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

---

[mbuffer]: https://www.maier-komor.de/mbuffer.html