//! Encrypted ZFS snapshot backups on S3-compatible object storage.
//!
//! See `docs/design.md` for the module map and design rationale.

pub mod backup;
pub mod cli;
pub mod crypto;
pub mod model;
pub mod process;
pub mod rate;
pub mod restore;
pub mod s3;
pub mod store;
pub mod zfs;

#[cfg(any(test, feature = "test-support"))]
pub mod testing;
