# snapshot-to-s3

Encrypt snapshot of your modern file system and upload to S3-compatible object storage.

## Features

* Stream processing to prevent additional disk usage and risk of plaintext leak
* ZFS on Linux currently supported (more can be easily added later)
* Single backup file writes to a S3-compatible storage
* Each uploaded backup is encrypted with AES-256-GCM with a new key
* Use your favorite GPG key as KEK (key-encryption-key)
* Auto detect incremental backup