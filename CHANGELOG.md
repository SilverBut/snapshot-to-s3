# Changelog

## [Unreleased]

## [0.2.0]

### Fixed

- Send an explicit Content-Length (including zero) on empty PUT and POST requests, so backups no longer fail with HTTP 411 MissingContentLength on S3 endpoints that require it (#11). (#11)
- Report backup progress and current speed as request-body bytes are sent, rather than in spikes after each part finishes uploading (#19). (#19)

### Added

- Add a `--lock-detection-mode` option with `if-none-match` (default), Tencent COS `x-cos-forbid-overwrite` support, and an unsafe `dangerously-skip` mode that bypasses lock acquisition (#12). (#12)
- Add `RUST_LOG` diagnostic logging to stderr, a `--force-overwrite` option that deletes existing objects under the prefix, and a `--progress` option for backup and restore (#13). (#13)
- Unify part sizing under `--max-part-size` (default 512 MiB), remove `--part-buffer-size`, and add `--part-temp-file PATH` to spool encrypted parts to a temporary file instead of memory (#14). (#14)
- Verify S3 metadata after lock acquisition, keep committed backups with missing or wrong metadata, exit nonzero with repair diagnostics, and add an opt-in `--probe-metadata-multipart` option (#16). (#16)

### Upgrade notes

- Publish the release binary `snapshot-to-s3-linux-x86_64` directly alongside `SHA256SUMS` instead of a tar archive (#17). (#17)
- Rename the lock and metadata-probe objects from `.lock`, `.metadata-probe` and `.metadata-probe-multipart` to `_lock`, `_metadata-probe` and `_metadata-probe-multipart`, and add more detailed decision logging across lock, prefix, backup and metadata-probe flows (#18). (#18)


## [0.1.0]

### Added

- Add a Rust CLI foundation with `upload` and `download` subcommands, plus CI, security scanning and tag-triggered static musl release builds with SHA256SUMS. (#1)
- Add the snapshot-to-s3 backup system: ZFS snapshot sources, S3 multipart streaming upload, AES-256-GCM and GPG encryption, and rate limiting, using URI syntax for sources and destinations. (#2)
- Add UI-driven release preparation with explicit version bump and channel selection, an editable CHANGELOG with a notes-review CI guard, and verified static musl artifacts published as drafts, with a build-only validation mode. (#3)
- Stream oversized backups at petabyte scale by splitting them into continuation objects (`stream.encrypted.NNNNNN`) with memory bounded by part buffers and no temporary files. (#6)
- Generate draft release notes with the Copilot Release Notes action and feed them into the reviewed CHANGELOG release proposal, failing explicitly on empty output. (#8)

### Upgrade notes

- Make releases manual-only: publishing now requires dispatching the Release workflow in `publish` mode on `main`, and CI completion no longer triggers a Release. (#6)
- Remove instance metadata credential support, so S3 credentials now come only from environment variables or the shared credentials file, and a missing credential fails immediately when the S3 client is created. (#7)


### Needs Review
- Prepare a cloud Copilot development environment with preinstalled toolchain and a retained ZFS development pool, and add cloud setup to the required CI gate. (#5) — _Mainly contributor and development infrastructure, so it may not be user-visible and may belong in the omitted internal category._
