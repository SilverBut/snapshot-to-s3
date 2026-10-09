# Changelog

## [Unreleased]

## [0.1.0]

<!-- RELEASE_NOTES_NEED_REVIEW -->

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
